//! Durable PTY-output and input-WAL ownership for the standalone guardian.
//!
//! The steady-state PTY-output path hands one bounded, zeroizing plaintext
//! allocation to this fixed worker pool and receives only a content-free append
//! receipt after the encrypted journal record and its filesystem identity have
//! been synchronized and rechecked. Spawn preparation still performs bounded
//! filesystem publication before the child is admitted. There is deliberately
//! no plaintext getter or mux-delivery API here. The live-input worker owns the
//! per-pane encrypted WAL through the transaction wrappers below; the sole Mio
//! readiness loop never calls synchronous journal or PTY-write operations.

use crate::transport::provision_guardian_token_in_pinned_parent;
use mio::Waker;
use mux::guardian_checkpoint::{
    GUARDIAN_CHECKPOINT_ACK_FINALIZER_BYTES, GUARDIAN_CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_BYTES,
    GUARDIAN_CHECKPOINT_EXPIRY_FINALIZER_BYTES, GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES,
    GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES, GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
    GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES, GuardianCheckpointArtifactDescriptorV1,
    GuardianCheckpointBoundaryError, GuardianCheckpointCandidateIdentityV1,
    GuardianCheckpointCatalogAdoptionBindingV1, GuardianCheckpointCatalogAdoptionEvidenceV1,
    GuardianCheckpointCatalogPredecessorBindingV1, GuardianCheckpointCipher,
    GuardianCheckpointCipherError, GuardianCheckpointOrderedChunkSetBuilderV1,
    GuardianCheckpointOrderedChunkSetIdentityV1, GuardianCheckpointStageBindingV1,
    GuardianCheckpointStageRecordContextV1, GuardianCheckpointStageRecordKindV1,
    GuardianCheckpointStageScopeV1, GuardianCheckpointStageSealIntentV1,
    GuardianCheckpointValidatedManifestAuthorityV1, GuardianEncryptedCheckpointStageRecordV1,
    GuardianGenesisReservationIdentityV1,
};
use mux::guardian_input_journal::{
    GuardianInputCompletionError, GuardianInputJournal, GuardianInputJournalError,
    GuardianInputJournalLimits, GuardianInputTransaction, GuardianInputTransactionError,
    GuardianInputWriteOutcome, begin_guardian_input_transaction, commit_guardian_input_outcome,
    replay_guardian_input_without_writer,
};
use mux::guardian_output_journal::{
    GuardianOutputAppendReceipt, GuardianOutputCipher, GuardianOutputJournal,
    GuardianOutputJournalError, GuardianOutputJournalLimits, GuardianOutputJournalTail,
    GuardianOutputKey, GuardianOutputPredecessor, GuardianOutputSegmentIdentity,
};
use mux::guardian_protocol::{
    AuthenticatedGuardianRequest, GUARDIAN_MAX_CHECKPOINT_BYTES, GUARDIAN_MAX_CHECKPOINT_CHUNKS,
    GuardianCheckpointCatalogAdoptionEvidenceSeedV1, GuardianCheckpointCatalogAdoptionPermitV1,
    GuardianCheckpointDescriptorV1, GuardianCheckpointPolicyExpiryReceiptV1,
    GuardianCheckpointReceipt, GuardianCheckpointRuntimeSealPermitV1, GuardianCheckpointScopeV1,
    GuardianCheckpointStageKindV1, GuardianCheckpointStageReplyV1,
    GuardianCheckpointStageRequestV1, GuardianEffectTransactionError, GuardianProtocolError,
    GuardianProtocolState, GuardianReply,
};
use nix::unistd::{PathconfVar, fpathconf, geteuid};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use thiserror::Error;
use uuid::Uuid;
use zeroize::{Zeroize as _, Zeroizing};

pub const OUTPUT_RECORD_BYTES: usize = 8 * 1024;
const OUTPUT_DIRECTORY_NAME: &str = "guardian-output-v3";
const OUTPUT_KEY_NAME: &str = "journal.key";
const INPUT_JOURNAL_SUFFIX: &str = "ftgin";
const OUTPUT_WORKER_THREADS: usize = 2;
const OUTPUT_MAX_IN_FLIGHT: usize = 64;
const OUTPUT_MANIFEST_CHECKSUM_DOMAIN: &[u8] = b"frankenterm.guardian-output-manifest.v1\0";
const OUTPUT_MANIFEST_MAGIC: [u8; 8] = *b"FTGOMF01";
const OUTPUT_MANIFEST_VERSION: u32 = 1;
const OUTPUT_MANIFEST_HEADER_BYTES: usize = 132;
const OUTPUT_MANIFEST_SEGMENT_BYTES: usize = 104;
const OUTPUT_MANIFEST_CHECKSUM_BYTES: usize = 32;
const OUTPUT_V3_FILE_HEADER_BYTES: u64 = 176;
const OUTPUT_V3_RECORD_OVERHEAD_BYTES: u64 = 96 + 16;
const OUTPUT_SEGMENT_LOG_BYTES: u64 = 16 * 1024 * 1024;
const OUTPUT_SEGMENT_MAX_RECORDS: u64 = 2_048;
const OUTPUT_MAX_SEGMENTS_PER_PANE: usize = 64;
const OUTPUT_MAX_DURABLE_BYTES_PER_PANE: u64 = 1024 * 1024 * 1024;
// Every committed segment owns a segment, manifest, and publication marker;
// the fourth slot retains one bounded crash candidate without reclamation.
const OUTPUT_MAX_RELEVANT_FILES_PER_PANE: usize = OUTPUT_MAX_SEGMENTS_PER_PANE * 4;
const OUTPUT_MAX_DIRECTORY_ENTRIES_PER_SCAN: usize = 1_048_576;
const OUTPUT_PATH_COLLISION_ATTEMPTS: usize = 8;
const CHECKPOINT_STAGE_FILE_PREFIX: &[u8] = b"checkpoint-";
const CHECKPOINT_STAGE_FILE_SUFFIX: &str = ".ftgcp";
const CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES: usize = 336;
const CHECKPOINT_STAGE_SEAL_PLAINTEXT_BYTES: usize = 400;
const CHECKPOINT_STAGE_ACK_PLAINTEXT_BYTES: usize = 392;
const CHECKPOINT_STAGE_EXPIRY_PLAINTEXT_BYTES: usize = 376;
const CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES: u64 = 296;
const CHECKPOINT_STAGE_MAX_FILES_PER_UPLOAD: usize = 1_027;
const CHECKPOINT_STAGE_MAX_BYTES_PER_UPLOAD: u64 = 268_740_576;
// Production Stage, catalog publication, and Query use this bounded retention
// today. Phase A has no deletion or ACK-backed reclamation, so keep the limit
// deliberately finite while checkpoint ACK and upgrade activation stay off.
const CHECKPOINT_STAGE_MAX_RETAINED_UPLOADS: usize = 8;
const CHECKPOINT_STAGE_MAX_FILES: usize = 8_216;
const CHECKPOINT_STAGE_MAX_BYTES: u64 = 2_149_924_608;
const CHECKPOINT_CATALOG_FILE_PREFIX: &str = "checkpoint-catalog-";
const CHECKPOINT_CATALOG_CANDIDATE_SUFFIX: &str = ".ftgccandidate";
const CHECKPOINT_CATALOG_MARKER_SUFFIX: &str = ".ftgccommit";
const CHECKPOINT_CATALOG_STAGING_SUFFIX: &str = ".staging";
const CHECKPOINT_CATALOG_LEGACY_CANDIDATE_MAGIC: [u8; 8] = *b"FTGCC002";
const CHECKPOINT_CATALOG_LEGACY_MARKER_MAGIC: [u8; 8] = *b"FTGCM002";
const CHECKPOINT_CATALOG_LEGACY_VERSION: u32 = 2;
const CHECKPOINT_CATALOG_CANDIDATE_MAGIC: [u8; 8] = *b"FTGCC003";
const CHECKPOINT_CATALOG_MARKER_MAGIC: [u8; 8] = *b"FTGCM003";
const CHECKPOINT_CATALOG_VERSION: u32 = 3;
const CHECKPOINT_CATALOG_HEADER_BYTES: usize = 568;
const CHECKPOINT_CATALOG_MARKER_BODY_BYTES: usize = 312;
const CHECKPOINT_CATALOG_MARKER_BYTES: usize =
    CHECKPOINT_CATALOG_MARKER_BODY_BYTES + OUTPUT_MANIFEST_CHECKSUM_BYTES;
const CHECKPOINT_CATALOG_LEGACY_CHECKSUM_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-catalog-candidate.v2\0";
const CHECKPOINT_CATALOG_LEGACY_MARKER_CHECKSUM_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-catalog-marker.v2\0";
const CHECKPOINT_CATALOG_CHECKSUM_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-catalog-candidate.v3\0";
const CHECKPOINT_CATALOG_MARKER_CHECKSUM_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-catalog-marker.v3\0";
const CHECKPOINT_CATALOG_GENESIS_CANDIDATE_ID_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-catalog-genesis-candidate-id.v1\0";
const CHECKPOINT_CATALOG_CANDIDATE_ID_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-catalog-candidate-id.v2\0";
const CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_RECORD_BYTES: u64 =
    GUARDIAN_CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_BYTES as u64
        + CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES;
const CHECKPOINT_CATALOG_MAX_PUBLISHED_MEMBERS: usize = 8;
// One immutable candidate and marker per retained member, plus one bounded
// crash candidate per generation. No reclamation is performed in this phase.
const CHECKPOINT_CATALOG_MAX_RELEVANT_FILES: usize = CHECKPOINT_CATALOG_MAX_PUBLISHED_MEMBERS * 3;
const CHECKPOINT_CATALOG_PROTOCOL_MAX_CHUNKS: u64 = 1_024;
const _: () = assert!(GUARDIAN_MAX_CHECKPOINT_CHUNKS == 1_024);
const CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES: u64 = GUARDIAN_MAX_CHECKPOINT_BYTES
    + (CHECKPOINT_CATALOG_PROTOCOL_MAX_CHUNKS + 2) * CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES
    + CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES as u64
    + CHECKPOINT_STAGE_SEAL_PLAINTEXT_BYTES as u64
    + CHECKPOINT_CATALOG_HEADER_BYTES as u64
    + OUTPUT_MANIFEST_CHECKSUM_BYTES as u64
    + CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_RECORD_BYTES;
const CHECKPOINT_CATALOG_MAX_RELEVANT_BYTES: u64 =
    CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES * 16 + CHECKPOINT_CATALOG_MARKER_BYTES as u64 * 8;

#[derive(Clone, Copy, Debug)]
struct GuardianCheckpointStagePolicy {
    max_retained_uploads: usize,
    max_stage_files: usize,
    max_stage_bytes: u64,
}

impl GuardianCheckpointStagePolicy {
    const fn production() -> Self {
        Self {
            max_retained_uploads: CHECKPOINT_STAGE_MAX_RETAINED_UPLOADS,
            max_stage_files: CHECKPOINT_STAGE_MAX_FILES,
            max_stage_bytes: CHECKPOINT_STAGE_MAX_BYTES,
        }
    }

    fn validate(self) -> Result<Self, GuardianOutputError> {
        let protocol_files_per_upload = usize::try_from(GUARDIAN_MAX_CHECKPOINT_CHUNKS)
            .ok()
            .and_then(|chunks| chunks.checked_add(3))
            .ok_or(GuardianOutputError::Allocation)?;
        let protocol_bytes_per_upload = u64::from(GUARDIAN_MAX_CHECKPOINT_CHUNKS)
            .checked_mul(CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES)
            .and_then(|overhead| overhead.checked_add(GUARDIAN_MAX_CHECKPOINT_BYTES))
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES)
                        .ok()?
                        .checked_add(CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES)?,
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(CHECKPOINT_STAGE_SEAL_PLAINTEXT_BYTES)
                        .ok()?
                        .checked_add(CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES)?,
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(CHECKPOINT_STAGE_ACK_PLAINTEXT_BYTES)
                        .ok()?
                        .checked_add(CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES)?,
                )
            })
            .ok_or(GuardianOutputError::Allocation)?;
        let minimum_files = self
            .max_retained_uploads
            .checked_mul(CHECKPOINT_STAGE_MAX_FILES_PER_UPLOAD)
            .ok_or(GuardianOutputError::Allocation)?;
        let minimum_bytes = u64::try_from(self.max_retained_uploads)
            .ok()
            .and_then(|uploads| uploads.checked_mul(CHECKPOINT_STAGE_MAX_BYTES_PER_UPLOAD))
            .ok_or(GuardianOutputError::Allocation)?;
        if self.max_retained_uploads == 0
            || protocol_files_per_upload != CHECKPOINT_STAGE_MAX_FILES_PER_UPLOAD
            || protocol_bytes_per_upload != CHECKPOINT_STAGE_MAX_BYTES_PER_UPLOAD
            || u32::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES).ok()
                != Some(GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES)
            || u32::try_from(CHECKPOINT_STAGE_SEAL_PLAINTEXT_BYTES).ok()
                != Some(GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)
            || CHECKPOINT_STAGE_ACK_PLAINTEXT_BYTES
                != usize::try_from(GUARDIAN_CHECKPOINT_ACK_FINALIZER_BYTES)
                    .map_err(|_| GuardianOutputError::Allocation)?
            || CHECKPOINT_STAGE_EXPIRY_PLAINTEXT_BYTES
                != usize::try_from(GUARDIAN_CHECKPOINT_EXPIRY_FINALIZER_BYTES)
                    .map_err(|_| GuardianOutputError::Allocation)?
            || CHECKPOINT_STAGE_EXPIRY_PLAINTEXT_BYTES > CHECKPOINT_STAGE_ACK_PLAINTEXT_BYTES
            || self.max_retained_uploads > CHECKPOINT_STAGE_MAX_RETAINED_UPLOADS
            || self.max_stage_files < minimum_files
            || self.max_stage_files > CHECKPOINT_STAGE_MAX_FILES
            || self.max_stage_bytes < minimum_bytes
            || self.max_stage_bytes > CHECKPOINT_STAGE_MAX_BYTES
        {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian checkpoint staging policy is invalid",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Error)]
pub enum GuardianOutputError {
    #[error("guardian output path is not absolute and normalized")]
    InvalidPath,
    #[error("guardian output filesystem authority is invalid: {0}")]
    FilesystemAuthority(&'static str),
    #[error("guardian output filesystem I/O failed at {site}")]
    Io {
        site: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("guardian output journal initialization failed")]
    Journal(#[from] GuardianOutputJournalError),
    #[error("guardian encrypted input journal initialization failed")]
    InputJournal(#[from] GuardianInputJournalError),
    #[error("guardian output worker allocation failed")]
    Allocation,
}

impl GuardianOutputError {
    fn io(site: &'static str, source: std::io::Error) -> Self {
        Self::Io { site, source }
    }
}

#[derive(Debug, Error)]
pub enum GuardianCheckpointStageStoreError {
    #[error("guardian checkpoint staging request is invalid")]
    Protocol(#[from] GuardianProtocolError),
    #[error("guardian checkpoint staging cipher rejected the record")]
    Cipher(#[from] GuardianCheckpointCipherError),
    #[error("guardian checkpoint staging boundary authority is invalid")]
    Boundary(#[from] GuardianCheckpointBoundaryError),
    #[error("guardian checkpoint staging output authority is invalid")]
    Output(#[from] GuardianOutputError),
    #[error("guardian checkpoint staging journal recovery failed")]
    Journal(#[from] GuardianOutputJournalError),
    #[error("guardian checkpoint staging filesystem I/O failed at {site}")]
    Io {
        site: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("guardian checkpoint staging store lock was poisoned")]
    LockPoisoned,
    #[error("guardian checkpoint staging upload conflicts with durable state")]
    Conflict,
    #[error("guardian checkpoint staging chunks are not a contiguous prefix")]
    OutOfOrder,
    #[error("guardian checkpoint staging upload is poisoned or incomplete")]
    Poisoned,
    #[error("guardian checkpoint staging resource policy is exhausted")]
    Capacity,
    #[error("guardian checkpoint staging allocation failed")]
    Allocation,
    #[error("guardian checkpoint staging filesystem does not expose a safe name bound")]
    NameLimit,
    #[error("guardian checkpoint staging operation has no durable candidate")]
    CandidateAbsent,
    #[error("guardian checkpoint staging origin authority does not match its scope")]
    OriginAuthorityMismatch,
}

impl GuardianCheckpointStageStoreError {
    fn io(site: &'static str, source: std::io::Error) -> Self {
        Self::Io { site, source }
    }
}

// The store accepts final publication only when the runtime supplies the
// independent non-forgeable live-capture or Genesis authority.
#[allow(dead_code)]
pub enum GuardianCheckpointOriginAuthority<'a> {
    /// A nonconstructible live-capture authority plus independent recovery of
    /// the exact output-journal receipt named by the canonical descriptor.
    /// A wire descriptor or caller-supplied receipt is never sufficient.
    Record {
        journal: &'a GuardianPaneOutputJournal,
        manifest_authority: GuardianCheckpointValidatedManifestAuthorityV1,
    },
    /// Genesis remains unavailable until Spawn retains and transports the
    /// guardian-issued permit needed to mint this opaque authority. A raw
    /// effect UUID is deliberately not accepted here.
    Genesis {
        manifest_authority: GuardianCheckpointValidatedManifestAuthorityV1,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CheckpointStagePathScope {
    Pane { pane_id: Uuid, generation: u64 },
    Genesis { spawn_effect_id: Uuid },
}

impl CheckpointStagePathScope {
    fn from_protocol(scope: GuardianCheckpointScopeV1) -> Self {
        match scope {
            GuardianCheckpointScopeV1::Pane {
                pane_id,
                generation,
            } => Self::Pane {
                pane_id,
                generation,
            },
            GuardianCheckpointScopeV1::Genesis { spawn_effect_id } => {
                Self::Genesis { spawn_effect_id }
            }
        }
    }

    fn stage_scope(self) -> Result<GuardianCheckpointStageScopeV1, GuardianCheckpointCipherError> {
        match self {
            Self::Pane {
                pane_id,
                generation,
            } => GuardianCheckpointStageScopeV1::pane(pane_id, generation),
            Self::Genesis { spawn_effect_id } => {
                GuardianCheckpointStageScopeV1::genesis(spawn_effect_id)
            }
        }
    }

    fn base_name(self, upload_id: Uuid) -> String {
        match self {
            Self::Pane {
                pane_id,
                generation,
            } => {
                format!("checkpoint-pane-{pane_id}.generation-{generation:020}.upload-{upload_id}")
            }
            Self::Genesis { spawn_effect_id } => {
                format!("checkpoint-genesis-{spawn_effect_id}.upload-{upload_id}")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CheckpointStageUploadKey {
    scope: CheckpointStagePathScope,
    upload_id: Uuid,
}

impl CheckpointStageUploadKey {
    fn base_name(self) -> String {
        self.scope.base_name(self.upload_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CheckpointStageFileRole {
    Candidate,
    Chunk { publication_id: Uuid, index: u32 },
    Seal { publication_id: Uuid },
    Ack { publication_id: Uuid },
    Expired { publication_id: Uuid },
}

#[derive(Debug)]
struct CheckpointStageCensusEntry {
    key: CheckpointStageUploadKey,
    role: CheckpointStageFileRole,
    path: PathBuf,
    bytes: u64,
}

struct CheckpointStageCensus {
    entries: Vec<CheckpointStageCensusEntry>,
    uploads: BTreeSet<CheckpointStageUploadKey>,
    total_files: usize,
    total_bytes: u64,
}

enum CheckpointStageCreateOutcome {
    Created(File),
    Existing,
}

struct CheckpointStageRequestShape {
    scope: GuardianCheckpointScopeV1,
    path_scope: CheckpointStagePathScope,
    upload_id: Uuid,
    descriptor: GuardianCheckpointDescriptorV1,
    canonical_descriptor: GuardianCheckpointArtifactDescriptorV1,
    binding: GuardianCheckpointStageBindingV1,
    chunk_bytes: u32,
    total_chunks: u32,
    total_bytes: u64,
}

impl CheckpointStageRequestShape {
    fn from_request(
        request: &GuardianCheckpointStageRequestV1,
    ) -> Result<Self, GuardianCheckpointStageStoreError> {
        let scope = request.scope();
        let path_scope = CheckpointStagePathScope::from_protocol(scope);
        let descriptor = request.descriptor();
        let canonical_descriptor = descriptor.canonical_descriptor()?;
        let binding = GuardianCheckpointStageBindingV1::from_protocol_capture(
            path_scope.stage_scope()?,
            canonical_descriptor,
            descriptor.capture_generation(),
        )?;
        Ok(Self {
            scope,
            path_scope,
            upload_id: request.upload_id(),
            descriptor,
            canonical_descriptor,
            binding,
            chunk_bytes: request.chunk_bytes(),
            total_chunks: request.total_chunks(),
            total_bytes: request.total_bytes(),
        })
    }

    fn key(&self) -> CheckpointStageUploadKey {
        CheckpointStageUploadKey {
            scope: self.path_scope,
            upload_id: self.upload_id,
        }
    }

    fn begin_payload(&self) -> Result<Zeroizing<Vec<u8>>, GuardianProtocolError> {
        GuardianCheckpointStageRequestV1::begin(
            self.scope,
            self.upload_id,
            self.descriptor,
            self.chunk_bytes,
        )?
        .into_zeroizing_payload()
    }
}

struct CheckpointStageUploadInspection {
    publication_id: Uuid,
    next_index: u32,
    committed_bytes: u64,
    seal_present: bool,
    ack_present: bool,
    expiry_present: bool,
    candidate_identity: GuardianCheckpointCandidateIdentityV1,
    ordered_chunk_set_identity: Option<GuardianCheckpointOrderedChunkSetIdentityV1>,
}

#[derive(Clone, Copy)]
enum CheckpointStageSealInspection {
    Reject,
    IgnoreForHistoricalChunkRetry,
}

struct GuardianCheckpointStageStoreInner {
    directory: File,
    directory_path: PathBuf,
    cipher: GuardianCheckpointCipher,
    persistence: Arc<PersistentOutputAuthority>,
    policy: GuardianCheckpointStagePolicy,
    name_max: usize,
    gate: Mutex<()>,
    durable_records: Mutex<Vec<FileIdentity>>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CheckpointCatalogScope {
    Pane { pane_id: Uuid },
    Genesis { spawn_effect_id: Uuid },
}

impl CheckpointCatalogScope {
    fn from_stage_scope(scope: CheckpointStagePathScope) -> Self {
        match scope {
            CheckpointStagePathScope::Pane { pane_id, .. } => Self::Pane { pane_id },
            CheckpointStagePathScope::Genesis { spawn_effect_id } => {
                Self::Genesis { spawn_effect_id }
            }
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Pane { .. } => 1,
            Self::Genesis { .. } => 2,
        }
    }

    const fn identity(self) -> Uuid {
        match self {
            Self::Pane { pane_id } => pane_id,
            Self::Genesis { spawn_effect_id } => spawn_effect_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CheckpointCatalogPredecessor {
    generation: u64,
    candidate_id: Uuid,
    candidate_checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
    checkpoint_id: [u8; 32],
    boundary_id: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CheckpointCatalogIdentity {
    scope: CheckpointCatalogScope,
    generation: u64,
    candidate_id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CheckpointCatalogMetadata {
    identity: CheckpointCatalogIdentity,
    predecessor: Option<CheckpointCatalogPredecessor>,
    upload_id: Uuid,
    completion_id: Uuid,
    checkpoint_id: [u8; 32],
    boundary_id: [u8; 32],
    terminal_payload_digest: [u8; 32],
    total_bytes: u64,
    chunk_count: u32,
    capture_generation: u64,
    replay_semantics_id: [u8; 32],
    rows: u32,
    cols: u32,
    adoption_mux_incarnation: Uuid,
    adoption_effect_id: Uuid,
    adoption_sequence: u64,
    genesis_durable_pane_id: Uuid,
    genesis_origin_request_id: Uuid,
    genesis_spawn_payload_bytes: u64,
    genesis_spawn_payload_digest: [u8; 32],
    genesis_spawning_mux_build_identity_digest: [u8; 32],
    genesis_live_guardian_build_identity_digest: [u8; 32],
    genesis_pixel_width: u16,
    genesis_pixel_height: u16,
}

/// Guardian-private proof that one exact Genesis reservation is represented by
/// a checksum-bound catalog candidate and its synchronously durable marker.
///
/// The only production constructor is the publication path below, after its
/// post-marker directory sync and full catalog rescan. Keeping the complete
/// mux-issued reservation identity inside this nonduplicable value lets the
/// runtime consume the same authority when it finally opens the PTY.
#[must_use = "Genesis admission authority must be consumed by the Spawn runtime"]
#[allow(dead_code)] // Consumed by the immediately following runtime-wiring tranche.
pub struct GuardianPublishedGenesisAdmissionPermitV1 {
    reservation_identity: GuardianGenesisReservationIdentityV1,
    catalog_candidate_checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
}

#[allow(dead_code)] // Kept narrow until the runtime consumes this new authority.
impl GuardianPublishedGenesisAdmissionPermitV1 {
    pub(crate) const fn reservation_identity(&self) -> &GuardianGenesisReservationIdentityV1 {
        &self.reservation_identity
    }

    pub(crate) const fn catalog_candidate_checksum(&self) -> &[u8; OUTPUT_MANIFEST_CHECKSUM_BYTES] {
        &self.catalog_candidate_checksum
    }

    pub(crate) fn into_reservation_identity(self) -> GuardianGenesisReservationIdentityV1 {
        self.reservation_identity
    }
}

impl std::fmt::Debug for GuardianPublishedGenesisAdmissionPermitV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianPublishedGenesisAdmissionPermitV1")
            .field("reservation_identity", &self.reservation_identity)
            .field("catalog_candidate_checksum", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CheckpointCatalogGenesisReservationBinding {
    mux_incarnation: Uuid,
    spawn_effect_id: Uuid,
    durable_pane_id: Uuid,
    origin_request_id: Uuid,
    spawn_payload_bytes: u64,
    spawn_payload_digest: [u8; 32],
    spawning_mux_build_identity_digest: [u8; 32],
    live_guardian_build_identity_digest: [u8; 32],
    rows: u16,
    cols: u16,
    pixel_width: u16,
    pixel_height: u16,
    checkpoint_identity_digest: [u8; 32],
    boundary_identity_digest: [u8; 32],
    upload_id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckpointCatalogFormat {
    LegacyV2,
    ProtectedV3,
}

impl CheckpointCatalogFormat {
    fn from_candidate_header(
        magic: [u8; 8],
        version: u32,
    ) -> Result<Self, GuardianCheckpointStageStoreError> {
        match (magic, version) {
            (CHECKPOINT_CATALOG_LEGACY_CANDIDATE_MAGIC, CHECKPOINT_CATALOG_LEGACY_VERSION) => {
                Ok(Self::LegacyV2)
            }
            (CHECKPOINT_CATALOG_CANDIDATE_MAGIC, CHECKPOINT_CATALOG_VERSION) => {
                Ok(Self::ProtectedV3)
            }
            _ => Err(GuardianCheckpointStageStoreError::Poisoned),
        }
    }

    fn from_marker_header(
        magic: [u8; 8],
        version: u32,
    ) -> Result<Self, GuardianCheckpointStageStoreError> {
        match (magic, version) {
            (CHECKPOINT_CATALOG_LEGACY_MARKER_MAGIC, CHECKPOINT_CATALOG_LEGACY_VERSION) => {
                Ok(Self::LegacyV2)
            }
            (CHECKPOINT_CATALOG_MARKER_MAGIC, CHECKPOINT_CATALOG_VERSION) => Ok(Self::ProtectedV3),
            _ => Err(GuardianCheckpointStageStoreError::Poisoned),
        }
    }

    const fn candidate_magic(self) -> [u8; 8] {
        match self {
            Self::LegacyV2 => CHECKPOINT_CATALOG_LEGACY_CANDIDATE_MAGIC,
            Self::ProtectedV3 => CHECKPOINT_CATALOG_CANDIDATE_MAGIC,
        }
    }

    const fn marker_magic(self) -> [u8; 8] {
        match self {
            Self::LegacyV2 => CHECKPOINT_CATALOG_LEGACY_MARKER_MAGIC,
            Self::ProtectedV3 => CHECKPOINT_CATALOG_MARKER_MAGIC,
        }
    }

    const fn version(self) -> u32 {
        match self {
            Self::LegacyV2 => CHECKPOINT_CATALOG_LEGACY_VERSION,
            Self::ProtectedV3 => CHECKPOINT_CATALOG_VERSION,
        }
    }

    const fn candidate_checksum_domain(self) -> &'static [u8] {
        match self {
            Self::LegacyV2 => CHECKPOINT_CATALOG_LEGACY_CHECKSUM_DOMAIN,
            Self::ProtectedV3 => CHECKPOINT_CATALOG_CHECKSUM_DOMAIN,
        }
    }

    const fn marker_checksum_domain(self) -> &'static [u8] {
        match self {
            Self::LegacyV2 => CHECKPOINT_CATALOG_LEGACY_MARKER_CHECKSUM_DOMAIN,
            Self::ProtectedV3 => CHECKPOINT_CATALOG_MARKER_CHECKSUM_DOMAIN,
        }
    }

    const fn authorizes_scope(self, scope: CheckpointCatalogScope) -> bool {
        matches!(self, Self::ProtectedV3) || matches!(scope, CheckpointCatalogScope::Genesis { .. })
    }
}

struct CheckpointCatalogCandidate {
    format: CheckpointCatalogFormat,
    metadata: CheckpointCatalogMetadata,
    records: Vec<GuardianEncryptedCheckpointStageRecordV1>,
    checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
    adoption_evidence: Option<GuardianEncryptedCheckpointStageRecordV1>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CheckpointCatalogMarker {
    format: CheckpointCatalogFormat,
    identity: CheckpointCatalogIdentity,
    predecessor_generation: Option<u64>,
    predecessor_candidate_id: Uuid,
    predecessor_checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
    upload_id: Uuid,
    completion_id: Uuid,
    checkpoint_id: [u8; 32],
    boundary_id: [u8; 32],
    terminal_payload_digest: [u8; 32],
    candidate_checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
    adoption_mux_incarnation: Uuid,
    adoption_effect_id: Uuid,
    adoption_sequence: u64,
}

#[derive(Clone)]
struct PublishedCheckpointCatalogMember {
    format: CheckpointCatalogFormat,
    metadata: CheckpointCatalogMetadata,
    candidate_checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
    candidate_path: PathBuf,
    candidate_file_identity: FileIdentity,
    marker_path: PathBuf,
    marker_file_identity: FileIdentity,
}

struct CheckpointCatalogScan {
    published: Vec<PublishedCheckpointCatalogMember>,
    unpublished_candidates: Vec<DiscoveredCheckpointCatalogCandidate>,
    staged_files: Vec<DiscoveredCheckpointCatalogStagingFile>,
    relevant_files: usize,
    relevant_bytes: u64,
}

#[derive(Clone)]
struct DiscoveredCheckpointCatalogCandidate {
    identity: CheckpointCatalogIdentity,
    path: PathBuf,
    file_identity: FileIdentity,
    bytes: u64,
    published: bool,
}

struct DiscoveredCheckpointCatalogMarker {
    marker: CheckpointCatalogMarker,
    path: PathBuf,
    file_identity: FileIdentity,
}

#[derive(Clone)]
struct DiscoveredCheckpointCatalogStagingFile {
    identity: CheckpointCatalogIdentity,
    role: CheckpointCatalogPathRole,
    path: PathBuf,
    file_identity: FileIdentity,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckpointCatalogPathRole {
    Candidate,
    Marker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckpointCatalogPathKind {
    Canonical(CheckpointCatalogPathRole),
    Staging(CheckpointCatalogPathRole),
}

/// Descriptor-relative, synchronously durable Phase-A checkpoint staging.
///
/// These methods perform bounded filesystem and AEAD work and therefore must
/// run only on the dedicated checkpoint worker, never on the Mio readiness
/// loop. Clones share one process-local gate; a directory `flock` coordinates
/// cooperating guardian processes before deterministic `O_EXCL` publication.
#[derive(Clone)]
pub struct GuardianCheckpointStageStore {
    inner: Arc<GuardianCheckpointStageStoreInner>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FileIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    links: u64,
    expected_len: Option<u64>,
}

impl FileIdentity {
    fn capture(metadata: &Metadata, expected_len: Option<u64>) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            owner: metadata.uid(),
            links: metadata.nlink(),
            expected_len,
        }
    }

    fn matches(self, metadata: &Metadata) -> bool {
        self.device == metadata.dev()
            && self.inode == metadata.ino()
            && self.mode == metadata.mode()
            && self.owner == metadata.uid()
            && self.links == metadata.nlink()
            && self
                .expected_len
                .is_none_or(|expected| expected == metadata.len())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
}

impl DirectoryIdentity {
    fn capture(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            owner: metadata.uid(),
        }
    }

    fn matches(self, metadata: &Metadata) -> bool {
        self.device == metadata.dev()
            && self.inode == metadata.ino()
            && self.mode == metadata.mode()
            && self.owner == metadata.uid()
    }
}

#[derive(Debug)]
struct PersistentOutputAuthority {
    parent_path: PathBuf,
    parent_identity: DirectoryIdentity,
    directory_path: PathBuf,
    directory_identity: DirectoryIdentity,
    key_path: PathBuf,
    key_identity: FileIdentity,
}

impl PersistentOutputAuthority {
    fn validate(&self, directory: &File) -> Result<(), OutputCommitError> {
        validate_directory_path_identity(&self.parent_path, self.parent_identity)
            .map_err(|_| OutputCommitError::PersistenceAuthority)?;
        validate_directory_path_identity(&self.directory_path, self.directory_identity)
            .map_err(|_| OutputCommitError::PersistenceAuthority)?;
        let directory_metadata = directory
            .metadata()
            .map_err(|_| OutputCommitError::PersistenceAuthority)?;
        if !self.directory_identity.matches(&directory_metadata) {
            return Err(OutputCommitError::PersistenceAuthority);
        }
        validate_file_identity_at(
            directory,
            &self.directory_path,
            &self.key_path,
            self.key_identity,
        )
        .map_err(|_| OutputCommitError::PersistenceAuthority)
    }
}

#[derive(Clone, Copy, Debug)]
struct OutputSegmentPolicy {
    journal_limits: GuardianOutputJournalLimits,
    max_segments: usize,
    // Includes referenced and orphan segments, immutable manifest candidates,
    // and publication markers rather than counting only encrypted log frames.
    max_durable_pane_bytes: u64,
}

impl OutputSegmentPolicy {
    const fn production() -> Self {
        Self {
            journal_limits: GuardianOutputJournalLimits {
                max_record_bytes: 8 * 1024,
                max_log_bytes: OUTPUT_SEGMENT_LOG_BYTES,
                max_records: OUTPUT_SEGMENT_MAX_RECORDS,
            },
            max_segments: OUTPUT_MAX_SEGMENTS_PER_PANE,
            max_durable_pane_bytes: OUTPUT_MAX_DURABLE_BYTES_PER_PANE,
        }
    }

    fn validate(self) -> Result<Self, GuardianOutputError> {
        let maximum_record_bytes = usize::try_from(self.journal_limits.max_record_bytes)
            .map_err(|_| GuardianOutputError::Allocation)?;
        let initial_manifest_bytes = manifest_encoded_bytes(1)?;
        let minimum_total_bytes = minimum_segment_bytes(maximum_record_bytes)?
            .checked_add(initial_manifest_bytes)
            .ok_or(GuardianOutputError::Allocation)?;
        if self.max_segments == 0
            || self.max_segments > OUTPUT_MAX_SEGMENTS_PER_PANE
            || self.journal_limits.max_record_bytes == 0
            || maximum_record_bytes > OUTPUT_RECORD_BYTES
            || self.journal_limits.max_records == 0
            || self.journal_limits.max_log_bytes < minimum_segment_bytes(maximum_record_bytes)?
            || self.max_durable_pane_bytes < self.journal_limits.max_log_bytes
            || self.max_durable_pane_bytes < minimum_total_bytes
            || self
                .max_segments
                .checked_mul(3)
                .is_none_or(|files| files > OUTPUT_MAX_RELEVANT_FILES_PER_PANE)
        {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output segment policy is invalid",
            ));
        }
        Ok(self)
    }

    fn total_record_capacity(self) -> Result<u64, GuardianOutputError> {
        u64::try_from(self.max_segments)
            .ok()
            .and_then(|segments| segments.checked_mul(self.journal_limits.max_records))
            .ok_or(GuardianOutputError::Allocation)
    }
}

#[derive(Clone)]
struct SegmentPathAuthority {
    segment_identity: GuardianOutputSegmentIdentity,
    path: PathBuf,
    file_identity: FileIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ManifestPredecessor {
    manifest_id: Uuid,
    checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
}

#[derive(Clone)]
struct OutputManifestSnapshot {
    durable_pane_id: Uuid,
    guardian_incarnation: Uuid,
    manifest_id: Uuid,
    revision: u64,
    predecessor: Option<ManifestPredecessor>,
    segments: Vec<GuardianOutputSegmentIdentity>,
    checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
}

struct PublishedManifestAuthority {
    snapshot: OutputManifestSnapshot,
    path: PathBuf,
    file_identity: FileIdentity,
    publication_path: PathBuf,
    publication_file_identity: FileIdentity,
}

#[derive(Clone)]
struct ManifestPathAuthority {
    path: PathBuf,
    file_identity: FileIdentity,
    publication_path: PathBuf,
    publication_file_identity: FileIdentity,
}

struct PaneJournalAuthority {
    current_journal: GuardianOutputJournal,
    segments: Vec<SegmentPathAuthority>,
    manifest: PublishedManifestAuthority,
    manifest_history: Vec<ManifestPathAuthority>,
    directory: File,
    directory_path: PathBuf,
    cipher: GuardianOutputCipher,
    policy: OutputSegmentPolicy,
    persistence: Arc<PersistentOutputAuthority>,
    total_committed_log_bytes: u64,
    total_relevant_file_bytes: u64,
    total_records: u64,
    physical_segment_files: usize,
    relevant_files: usize,
    failed: bool,
}

impl PaneJournalAuthority {
    fn append_and_sync(
        &mut self,
        payload: &[u8],
    ) -> Result<GuardianOutputAppendReceipt, OutputCommitError> {
        if self.failed {
            return Err(OutputCommitError::SegmentManager);
        }
        let result = self.append_and_sync_once(payload);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn append_and_sync_once(
        &mut self,
        payload: &[u8],
    ) -> Result<GuardianOutputAppendReceipt, OutputCommitError> {
        self.validate_path_authority()?;
        if payload.is_empty()
            || u32::try_from(payload.len())
                .ok()
                .is_none_or(|bytes| bytes > self.policy.journal_limits.max_record_bytes)
        {
            return Err(OutputCommitError::Capacity);
        }
        if !journal_can_append(
            &self.current_journal,
            self.policy.journal_limits,
            payload.len(),
        )? {
            self.rollover(payload.len())?;
        }
        let frame_bytes = output_frame_bytes(payload.len())?;
        let projected_total_log = self
            .total_committed_log_bytes
            .checked_add(frame_bytes)
            .ok_or(OutputCommitError::Capacity)?;
        let projected_total_files = self
            .total_relevant_file_bytes
            .checked_add(frame_bytes)
            .ok_or(OutputCommitError::Capacity)?;
        if projected_total_files > self.policy.max_durable_pane_bytes {
            return Err(OutputCommitError::Capacity);
        }
        let expected_segment_id = self
            .segments
            .last()
            .ok_or(OutputCommitError::SegmentManager)?
            .segment_identity
            .segment_id();
        let receipt = self.current_journal.append_and_sync(payload)?;
        if receipt.segment_id() != expected_segment_id {
            return Err(OutputCommitError::SegmentManager);
        }
        self.total_committed_log_bytes = projected_total_log;
        self.total_relevant_file_bytes = projected_total_files;
        self.total_records = self
            .total_records
            .checked_add(1)
            .ok_or(OutputCommitError::Capacity)?;
        self.validate_path_authority()?;
        Ok(receipt)
    }

    fn validate_path_authority(&self) -> Result<(), OutputCommitError> {
        let minimum_relevant_files = self
            .physical_segment_files
            .checked_add(
                self.manifest_history
                    .len()
                    .checked_mul(2)
                    .ok_or(OutputCommitError::SegmentManager)?,
            )
            .ok_or(OutputCommitError::SegmentManager)?;
        let manifest_tail = self
            .manifest_history
            .last()
            .ok_or(OutputCommitError::SegmentManager)?;
        let maximum_records = self
            .policy
            .total_record_capacity()
            .map_err(|_| OutputCommitError::SegmentManager)?;
        if self.segments.is_empty()
            || self.segments.len() != self.manifest.snapshot.segments.len()
            || self.segments.len() != self.manifest_history.len()
            || self.physical_segment_files < self.segments.len()
            || self.physical_segment_files > self.policy.max_segments
            || self.relevant_files < minimum_relevant_files
            || self.relevant_files > OUTPUT_MAX_RELEVANT_FILES_PER_PANE
            || self.total_relevant_file_bytes > self.policy.max_durable_pane_bytes
            || self.total_committed_log_bytes > self.total_relevant_file_bytes
            || self.total_records > maximum_records
            || self.current_journal.is_poisoned()
            || self.current_journal.tail() != GuardianOutputJournalTail::Clean
            || self.current_journal.directory_entry_sync_required()
            || self.current_journal.identity()
                != self
                    .segments
                    .last()
                    .ok_or(OutputCommitError::SegmentManager)?
                    .segment_identity
            || self
                .segments
                .iter()
                .zip(&self.manifest.snapshot.segments)
                .any(|(path, manifest)| path.segment_identity != *manifest)
            || manifest_tail.path != self.manifest.path
            || manifest_tail.file_identity != self.manifest.file_identity
            || manifest_tail.publication_path != self.manifest.publication_path
            || manifest_tail.publication_file_identity != self.manifest.publication_file_identity
        {
            return Err(OutputCommitError::SegmentManager);
        }
        self.persistence.validate(&self.directory)?;
        validate_file_identity_at(
            &self.directory,
            &self.directory_path,
            &self.manifest.path,
            self.manifest.file_identity,
        )
        .map_err(|_| OutputCommitError::PersistenceAuthority)?;
        validate_file_identity_at(
            &self.directory,
            &self.directory_path,
            &self.manifest.publication_path,
            self.manifest.publication_file_identity,
        )
        .map_err(|_| OutputCommitError::PersistenceAuthority)?;
        for manifest in &self.manifest_history {
            validate_file_identity_at(
                &self.directory,
                &self.directory_path,
                &manifest.path,
                manifest.file_identity,
            )
            .map_err(|_| OutputCommitError::PersistenceAuthority)?;
            validate_file_identity_at(
                &self.directory,
                &self.directory_path,
                &manifest.publication_path,
                manifest.publication_file_identity,
            )
            .map_err(|_| OutputCommitError::PersistenceAuthority)?;
        }
        for segment in &self.segments {
            validate_file_identity_at(
                &self.directory,
                &self.directory_path,
                &segment.path,
                segment.file_identity,
            )
            .map_err(|_| OutputCommitError::PersistenceAuthority)?;
        }
        Ok(())
    }

    fn rollover(&mut self, payload_bytes: usize) -> Result<(), OutputCommitError> {
        if self.physical_segment_files >= self.policy.max_segments
            || self
                .relevant_files
                .checked_add(3)
                .is_none_or(|files| files > OUTPUT_MAX_RELEVANT_FILES_PER_PANE)
        {
            return Err(OutputCommitError::Capacity);
        }
        let terminal = self
            .current_journal
            .terminal_receipt()
            .ok_or(OutputCommitError::Capacity)?;
        let first_sequence = terminal
            .sequence()
            .checked_add(1)
            .ok_or(OutputCommitError::Capacity)?;
        let frame_bytes = output_frame_bytes(payload_bytes)?;
        let next_segment_count = self
            .segments
            .len()
            .checked_add(1)
            .ok_or(OutputCommitError::Capacity)?;
        let next_manifest_bytes =
            manifest_encoded_bytes(next_segment_count).map_err(|_| OutputCommitError::Capacity)?;
        let projected_total_files = self
            .total_relevant_file_bytes
            .checked_add(OUTPUT_V3_FILE_HEADER_BYTES)
            .and_then(|bytes| bytes.checked_add(next_manifest_bytes))
            .and_then(|bytes| bytes.checked_add(frame_bytes))
            .ok_or(OutputCommitError::Capacity)?;
        if projected_total_files > self.policy.max_durable_pane_bytes {
            return Err(OutputCommitError::Capacity);
        }
        validate_replayable_segment_chain(
            &self.directory,
            &self.directory_path,
            &self.segments,
            &self.cipher,
            self.policy,
        )
        .map_err(|_| OutputCommitError::SegmentManager)?;
        self.segments
            .try_reserve(1)
            .map_err(|_| OutputCommitError::SegmentManager)?;
        self.manifest_history
            .try_reserve(1)
            .map_err(|_| OutputCommitError::SegmentManager)?;
        let (successor_journal, successor_path) = create_collision_resistant_segment(
            &self.directory,
            &self.directory_path,
            self.manifest.snapshot.durable_pane_id,
            self.manifest.snapshot.guardian_incarnation,
            first_sequence,
            Some(terminal.into_predecessor()),
            self.cipher.clone(),
            self.policy.journal_limits,
        )
        .map_err(|_| OutputCommitError::SegmentManager)?;
        let mut identities = self.manifest.snapshot.segments.clone();
        identities
            .try_reserve(1)
            .map_err(|_| OutputCommitError::SegmentManager)?;
        identities.push(successor_path.segment_identity);
        let next_manifest = publish_successor_manifest(
            &self.directory,
            &self.directory_path,
            &self.manifest.snapshot,
            identities,
        )
        .map_err(|_| OutputCommitError::SegmentManager)?;

        self.total_committed_log_bytes = self
            .total_committed_log_bytes
            .checked_add(successor_journal.committed_bytes())
            .ok_or(OutputCommitError::Capacity)?;
        self.total_relevant_file_bytes = self
            .total_relevant_file_bytes
            .checked_add(successor_journal.committed_bytes())
            .and_then(|bytes| bytes.checked_add(next_manifest_bytes))
            .ok_or(OutputCommitError::Capacity)?;
        self.physical_segment_files = self
            .physical_segment_files
            .checked_add(1)
            .ok_or(OutputCommitError::Capacity)?;
        self.relevant_files = self
            .relevant_files
            .checked_add(3)
            .ok_or(OutputCommitError::Capacity)?;
        self.current_journal = successor_journal;
        self.segments.push(successor_path);
        self.manifest_history.push(ManifestPathAuthority {
            path: next_manifest.path.clone(),
            file_identity: next_manifest.file_identity,
            publication_path: next_manifest.publication_path.clone(),
            publication_file_identity: next_manifest.publication_file_identity,
        });
        self.manifest = next_manifest;
        Ok(())
    }

    fn receipt_is_current(&self, receipt: GuardianOutputAppendReceipt) -> bool {
        self.current_journal.terminal_receipt() == Some(receipt)
            && self.segments.last().is_some_and(|segment| {
                segment.segment_identity.segment_id() == receipt.segment_id()
            })
    }

    fn can_accept_min_record(&self) -> bool {
        if self.failed
            || self.current_journal.is_poisoned()
            || self.current_journal.tail() != GuardianOutputJournalTail::Clean
            || self.current_journal.directory_entry_sync_required()
        {
            return false;
        }
        let Ok(frame_bytes) = output_frame_bytes(1) else {
            return false;
        };
        if self
            .total_relevant_file_bytes
            .checked_add(frame_bytes)
            .is_none_or(|bytes| bytes > self.policy.max_durable_pane_bytes)
        {
            return false;
        }
        if journal_can_append(&self.current_journal, self.policy.journal_limits, 1).unwrap_or(false)
        {
            return true;
        }
        self.physical_segment_files < self.policy.max_segments
            && self
                .relevant_files
                .checked_add(3)
                .is_some_and(|files| files <= OUTPUT_MAX_RELEVANT_FILES_PER_PANE)
            && self.current_journal.terminal_receipt().is_some()
            && self.current_journal.next_sequence().is_some()
            && self
                .total_relevant_file_bytes
                .checked_add(OUTPUT_V3_FILE_HEADER_BYTES)
                .and_then(|bytes| {
                    manifest_encoded_bytes(self.segments.len().checked_add(1)?)
                        .ok()
                        .and_then(|manifest| bytes.checked_add(manifest))
                })
                .and_then(|bytes| bytes.checked_add(frame_bytes))
                .is_some_and(|bytes| bytes <= self.policy.max_durable_pane_bytes)
    }

    fn remaining_records(&self) -> Result<u64, GuardianOutputError> {
        self.policy
            .total_record_capacity()?
            .checked_sub(self.total_records)
            .ok_or(GuardianOutputError::FilesystemAuthority(
                "guardian output total record accounting is inconsistent",
            ))
    }
}

/// Process-local append handle for one pane's bounded immutable segment chain.
#[derive(Clone)]
pub struct GuardianPaneOutputJournal {
    authority: Arc<Mutex<PaneJournalAuthority>>,
    initial_next_sequence: Option<u64>,
    initial_cumulative_plaintext_bytes: u64,
    initial_remaining_records: u64,
}

impl GuardianPaneOutputJournal {
    pub(crate) fn receipt_is_current(&self, receipt: GuardianOutputAppendReceipt) -> bool {
        self.authority
            .lock()
            .map(|authority| authority.receipt_is_current(receipt))
            .unwrap_or(false)
    }

    pub(crate) fn can_accept_min_record(&self) -> bool {
        self.authority
            .lock()
            .map(|authority| authority.can_accept_min_record())
            .unwrap_or(false)
    }

    pub(crate) const fn initial_next_sequence(&self) -> Option<u64> {
        self.initial_next_sequence
    }

    pub(crate) const fn initial_cumulative_plaintext_bytes(&self) -> u64 {
        self.initial_cumulative_plaintext_bytes
    }

    pub(crate) const fn initial_remaining_records(&self) -> u64 {
        self.initial_remaining_records
    }

    fn validate_checkpoint_record_origin(
        &self,
        descriptor: &GuardianCheckpointArtifactDescriptorV1,
    ) -> Result<(), GuardianCheckpointStageStoreError> {
        let authority = self
            .authority
            .lock()
            .map_err(|_| GuardianCheckpointStageStoreError::LockPoisoned)?;
        authority
            .validate_path_authority()
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
        let origin = descriptor.origin();
        let durable_pane_id = origin
            .durable_pane_id()
            .ok_or(GuardianCheckpointStageStoreError::OriginAuthorityMismatch)?;
        let segment_id = origin
            .segment_id()
            .ok_or(GuardianCheckpointStageStoreError::OriginAuthorityMismatch)?;
        let output_sequence = origin
            .output_sequence()
            .ok_or(GuardianCheckpointStageStoreError::OriginAuthorityMismatch)?;
        if authority.manifest.snapshot.durable_pane_id != durable_pane_id {
            return Err(GuardianCheckpointStageStoreError::OriginAuthorityMismatch);
        }
        validate_replayable_segment_chain(
            &authority.directory,
            &authority.directory_path,
            &authority.segments,
            &authority.cipher,
            authority.policy,
        )?;
        let segment = authority
            .segments
            .iter()
            .find(|segment| segment.segment_identity.segment_id() == segment_id)
            .ok_or(GuardianCheckpointStageStoreError::OriginAuthorityMismatch)?;
        validate_file_identity_at(
            &authority.directory,
            &authority.directory_path,
            &segment.path,
            segment.file_identity,
        )?;
        let file = open_private_file_at(
            &authority.directory,
            &authority.directory_path,
            &segment.path,
            false,
        )?;
        let journal = GuardianOutputJournal::open(
            file,
            segment.segment_identity,
            authority.cipher.clone(),
            authority.policy.journal_limits,
        )?;
        let mut cursor = journal.recovery_cursor(
            output_sequence,
            authority.policy.journal_limits.max_record_bytes,
        )?;
        let recovered = cursor
            .next_record()?
            .ok_or(GuardianCheckpointStageStoreError::OriginAuthorityMismatch)?;
        let receipt = recovered.receipt();
        descriptor.validate_record_authority(segment.segment_identity, receipt)?;
        drop(recovered);
        authority
            .validate_path_authority()
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
        Ok(())
    }
}

#[allow(dead_code)]
impl GuardianCheckpointStageStore {
    fn open(
        directory: &File,
        directory_path: &Path,
        output_cipher: &GuardianOutputCipher,
        persistence: Arc<PersistentOutputAuthority>,
        policy: GuardianCheckpointStagePolicy,
    ) -> Result<Self, GuardianOutputError> {
        let policy = policy.validate()?;
        persistence.validate(directory).map_err(|_| {
            GuardianOutputError::FilesystemAuthority(
                "guardian checkpoint persistence authority changed during initialization",
            )
        })?;
        let name_max = checkpoint_stage_name_max(directory)?;
        if name_max
            < checkpoint_stage_longest_name_bytes().max(checkpoint_catalog_longest_name_bytes())
        {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian checkpoint filenames exceed the pinned directory name bound",
            ));
        }
        Ok(Self {
            inner: Arc::new(GuardianCheckpointStageStoreInner {
                directory: directory.try_clone().map_err(|error| {
                    GuardianOutputError::io("checkpoint-directory-clone", error)
                })?,
                directory_path: directory_path.to_path_buf(),
                cipher: GuardianCheckpointCipher::from_output_cipher(output_cipher),
                persistence,
                policy,
                name_max,
                gate: Mutex::new(()),
                durable_records: Mutex::new(Vec::new()),
            }),
        })
    }

    pub(crate) fn apply_begin(
        &self,
        request: &GuardianCheckpointStageRequestV1,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianCheckpointStageStoreError> {
        if request.kind() != GuardianCheckpointStageKindV1::Begin {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        let shape = CheckpointStageRequestShape::from_request(request)?;
        let begin_payload = shape.begin_payload()?;
        if begin_payload.len() != CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        self.with_exclusive_directory(|inner| {
            let mut census = checkpoint_stage_census(inner)?;
            let has_candidate = census.entries.iter().any(|entry| {
                entry.key == shape.key() && entry.role == CheckpointStageFileRole::Candidate
            });
            let has_any = census.entries.iter().any(|entry| entry.key == shape.key());
            if !has_candidate {
                if has_any {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                let publication_id = Uuid::new_v4();
                let record_bytes = checkpoint_record_bytes_for_plaintext(begin_payload.len())?;
                checkpoint_stage_require_capacity(inner, &census, shape.key(), 1, record_bytes)?;
                let path = checkpoint_candidate_path(inner, shape.key())?;
                match checkpoint_create_record_new(inner, &path)? {
                    CheckpointStageCreateOutcome::Created(file) => {
                        let intent = GuardianCheckpointStageSealIntentV1::candidate_metadata(
                            &shape.binding,
                            shape.upload_id,
                            publication_id,
                            begin_payload,
                        )?;
                        let record = inner.cipher.seal(intent)?;
                        checkpoint_write_created_record(inner, &path, file, &record)?;
                    }
                    CheckpointStageCreateOutcome::Existing => {}
                }
                census = checkpoint_stage_census(inner)?;
            }
            let inspection = checkpoint_inspect_upload(
                inner,
                &census,
                &shape,
                CheckpointStageSealInspection::Reject,
            )?
            .ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
            Ok(GuardianCheckpointStageReplyV1::Ready {
                upload_id: shape.upload_id,
                next_index: inspection.next_index,
                committed_bytes: inspection.committed_bytes,
            })
        })
    }

    pub(crate) fn apply_chunk(
        &self,
        request: GuardianCheckpointStageRequestV1,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianCheckpointStageStoreError> {
        if request.kind() != GuardianCheckpointStageKindV1::Chunk {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        let shape = CheckpointStageRequestShape::from_request(&request)?;
        let chunk = request.into_chunk()?;
        let ((index, offset), bytes) = chunk
            .into_validated_parts()
            .map_err(GuardianCheckpointStageStoreError::Protocol)?;
        self.with_exclusive_directory(|inner| {
            let mut census = checkpoint_stage_census(inner)?;
            let mut inspection = checkpoint_inspect_upload(
                inner,
                &census,
                &shape,
                CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
            )?
            .ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
            if index < inspection.next_index {
                checkpoint_validate_exact_chunk_retry(
                    inner,
                    &census,
                    &shape,
                    inspection.publication_id,
                    index,
                    offset,
                    bytes,
                )?;
                return checkpoint_chunk_progress(&shape, index);
            }
            if index != inspection.next_index || inspection.seal_present {
                return Err(GuardianCheckpointStageStoreError::OutOfOrder);
            }
            let record_bytes = checkpoint_record_bytes_for_plaintext(bytes.len())?;
            checkpoint_stage_require_capacity(inner, &census, shape.key(), 1, record_bytes)?;
            let path = checkpoint_chunk_path(inner, shape.key(), inspection.publication_id, index)?;
            match checkpoint_create_record_new(inner, &path)? {
                CheckpointStageCreateOutcome::Created(file) => {
                    let intent = GuardianCheckpointStageSealIntentV1::chunk(
                        &shape.binding,
                        shape.upload_id,
                        inspection.publication_id,
                        index,
                        offset,
                        bytes,
                    )?;
                    let record = inner.cipher.seal(intent)?;
                    checkpoint_write_created_record(inner, &path, file, &record)?;
                }
                CheckpointStageCreateOutcome::Existing => {
                    census = checkpoint_stage_census(inner)?;
                    checkpoint_validate_exact_chunk_retry(
                        inner,
                        &census,
                        &shape,
                        inspection.publication_id,
                        index,
                        offset,
                        bytes,
                    )?;
                }
            }
            census = checkpoint_stage_census(inner)?;
            inspection = checkpoint_inspect_upload(
                inner,
                &census,
                &shape,
                CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
            )?
            .ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
            if inspection.next_index <= index {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            checkpoint_chunk_progress(&shape, index)
        })
    }

    pub(crate) fn apply_query(
        &self,
        request: GuardianCheckpointStageRequestV1,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianCheckpointStageStoreError> {
        if request.kind() != GuardianCheckpointStageKindV1::Query {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        let shape = CheckpointStageRequestShape::from_request(&request)?;
        self.with_exclusive_directory(|inner| {
            let recovered = (|| {
                let census = checkpoint_stage_census(inner)?;
                if !census.entries.iter().any(|entry| entry.key == shape.key()) {
                    return Ok(GuardianCheckpointStageReplyV1::Absent {
                        upload_id: shape.upload_id,
                    });
                }
                let inspection = checkpoint_inspect_upload(
                    inner,
                    &census,
                    &shape,
                    CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
                )?
                .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                if !inspection.seal_present {
                    if inspection.next_index == 0 {
                        return Ok(GuardianCheckpointStageReplyV1::Ready {
                            upload_id: shape.upload_id,
                            next_index: 0,
                            committed_bytes: 0,
                        });
                    }
                    return Ok(GuardianCheckpointStageReplyV1::Progress {
                        upload_id: shape.upload_id,
                        next_index: inspection.next_index,
                        committed_bytes: inspection.committed_bytes,
                    });
                }
                if inspection.next_index != shape.total_chunks
                    || inspection.committed_bytes != shape.total_bytes
                {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                let ordered_chunk_set_identity = inspection
                    .ordered_chunk_set_identity
                    .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                let seal_request = GuardianCheckpointStageRequestV1::seal(
                    shape.scope,
                    shape.upload_id,
                    shape.descriptor,
                    shape.chunk_bytes,
                )?;
                let seal_entry = census
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.key == shape.key()
                            && entry.role
                                == (CheckpointStageFileRole::Seal {
                                    publication_id: inspection.publication_id,
                                })
                    })
                    .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                let (_, record, _) = checkpoint_read_record(
                    inner,
                    seal_entry,
                    GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES,
                )?;
                let completion_receipt = inner.cipher.inspect_durable_manifest_receipt(
                    &shape.binding,
                    seal_request,
                    inspection.publication_id,
                    inspection.candidate_identity,
                    ordered_chunk_set_identity,
                    &record,
                )?;
                let payload =
                    checkpoint_assemble_payload(inner, &census, &shape, inspection.publication_id)?;
                request.validate_staged_plaintext(payload.as_slice())?;
                if inspection.ack_present {
                    let ack_request = GuardianCheckpointStageRequestV1::ack(
                        shape.scope,
                        shape.upload_id,
                        shape.descriptor,
                        shape.chunk_bytes,
                        inspection.publication_id,
                    )?;
                    let ack_entry = census
                        .entries
                        .iter()
                        .find(|entry| {
                            entry.key == shape.key()
                                && entry.role
                                    == (CheckpointStageFileRole::Ack {
                                        publication_id: inspection.publication_id,
                                    })
                        })
                        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                    let (_, ack_record, _) = checkpoint_read_record(
                        inner,
                        ack_entry,
                        u32::try_from(CHECKPOINT_STAGE_ACK_PLAINTEXT_BYTES)
                            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
                    )?;
                    inner.cipher.inspect_ack_finalizer(
                        &completion_receipt,
                        &ack_request,
                        &ack_record,
                    )?;
                    return Ok(GuardianCheckpointStageReplyV1::Acked {
                        upload_id: shape.upload_id,
                        completion_id: inspection.publication_id,
                        checkpoint_id: shape.descriptor.checkpoint_id(),
                        boundary_id: shape.descriptor.boundary_id(),
                        total_bytes: shape.total_bytes,
                    });
                }
                if inspection.expiry_present {
                    let expiry_entry = census
                        .entries
                        .iter()
                        .find(|entry| {
                            entry.key == shape.key()
                                && entry.role
                                    == (CheckpointStageFileRole::Expired {
                                        publication_id: inspection.publication_id,
                                    })
                        })
                        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                    let (_, expiry_record, _) = checkpoint_read_record(
                        inner,
                        expiry_entry,
                        u32::try_from(CHECKPOINT_STAGE_EXPIRY_PLAINTEXT_BYTES)
                            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
                    )?;
                    inner.cipher.inspect_expiry_finalizer(
                        &completion_receipt,
                        &request,
                        &expiry_record,
                    )?;
                    return Ok(GuardianCheckpointStageReplyV1::Expired {
                        upload_id: shape.upload_id,
                        completion_id: inspection.publication_id,
                        checkpoint_id: shape.descriptor.checkpoint_id(),
                        boundary_id: shape.descriptor.boundary_id(),
                        total_bytes: shape.total_bytes,
                    });
                }
                Ok(GuardianCheckpointStageReplyV1::Sealed {
                    upload_id: shape.upload_id,
                    completion_id: inspection.publication_id,
                    checkpoint_id: shape.descriptor.checkpoint_id(),
                    boundary_id: shape.descriptor.boundary_id(),
                    total_bytes: shape.total_bytes,
                })
            })();
            match recovered {
                Err(
                    GuardianCheckpointStageStoreError::Poisoned
                    | GuardianCheckpointStageStoreError::Cipher(_),
                ) => Ok(GuardianCheckpointStageReplyV1::Quarantined {
                    upload_id: shape.upload_id,
                }),
                result => result,
            }
        })
    }

    pub(crate) fn apply_ack(
        &self,
        request: GuardianCheckpointStageRequestV1,
        adoption_receipt: GuardianCheckpointReceipt,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianCheckpointStageStoreError> {
        if request.kind() != GuardianCheckpointStageKindV1::Ack {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        let shape = CheckpointStageRequestShape::from_request(&request)?;
        let requested_completion = request
            .completion_id()
            .ok_or(GuardianCheckpointStageStoreError::Conflict)?;
        self.with_exclusive_directory(|inner| {
            let census = checkpoint_stage_census(inner)?;
            let inspection = checkpoint_inspect_upload(
                inner,
                &census,
                &shape,
                CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
            )?
            .ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
            if !inspection.seal_present
                || inspection.next_index != shape.total_chunks
                || inspection.committed_bytes != shape.total_bytes
            {
                return Err(GuardianCheckpointStageStoreError::OutOfOrder);
            }
            if inspection.expiry_present {
                return Err(GuardianCheckpointStageStoreError::Conflict);
            }
            if requested_completion != inspection.publication_id {
                return Err(GuardianCheckpointStageStoreError::Conflict);
            }
            let ordered_chunk_set_identity = inspection
                .ordered_chunk_set_identity
                .ok_or(GuardianCheckpointStageStoreError::OutOfOrder)?;
            let payload =
                checkpoint_assemble_payload(inner, &census, &shape, inspection.publication_id)?;
            request.validate_staged_plaintext(payload.as_slice())?;
            let seal_request = GuardianCheckpointStageRequestV1::seal(
                shape.scope,
                shape.upload_id,
                shape.descriptor,
                shape.chunk_bytes,
            )?;
            let seal_entry = census
                .entries
                .iter()
                .find(|entry| {
                    entry.key == shape.key()
                        && entry.role
                            == (CheckpointStageFileRole::Seal {
                                publication_id: inspection.publication_id,
                            })
                })
                .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
            let (_, seal_record, _) =
                checkpoint_read_record(inner, seal_entry, GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)?;
            let completion_receipt = inner.cipher.inspect_durable_manifest_receipt(
                &shape.binding,
                seal_request,
                inspection.publication_id,
                inspection.candidate_identity,
                ordered_chunk_set_identity,
                &seal_record,
            )?;
            let ack_path = checkpoint_ack_path(inner, shape.key(), inspection.publication_id)?;
            let ack_plaintext_bytes = u32::try_from(CHECKPOINT_STAGE_ACK_PLAINTEXT_BYTES)
                .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
            if inspection.ack_present {
                let ack_entry = census
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.key == shape.key()
                            && entry.role
                                == (CheckpointStageFileRole::Ack {
                                    publication_id: inspection.publication_id,
                                })
                    })
                    .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                let (_, ack_record, _) =
                    checkpoint_read_record(inner, ack_entry, ack_plaintext_bytes)?;
                inner.cipher.inspect_ack_finalizer_with_adoption(
                    &completion_receipt,
                    &request,
                    adoption_receipt,
                    &ack_record,
                )?;
            } else {
                let record_bytes =
                    checkpoint_record_bytes_for_plaintext(CHECKPOINT_STAGE_ACK_PLAINTEXT_BYTES)?;
                checkpoint_stage_require_capacity(inner, &census, shape.key(), 1, record_bytes)?;
                match checkpoint_create_record_new(inner, &ack_path)? {
                    CheckpointStageCreateOutcome::Created(file) => {
                        let record = inner.cipher.seal_ack_finalizer(
                            &completion_receipt,
                            &request,
                            adoption_receipt,
                        )?;
                        checkpoint_write_created_record(inner, &ack_path, file, &record)?;
                    }
                    CheckpointStageCreateOutcome::Existing => {
                        let refreshed = checkpoint_stage_census(inner)?;
                        let ack_entry = refreshed
                            .entries
                            .iter()
                            .find(|entry| {
                                entry.key == shape.key()
                                    && entry.role
                                        == (CheckpointStageFileRole::Ack {
                                            publication_id: inspection.publication_id,
                                        })
                            })
                            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                        let (_, ack_record, _) =
                            checkpoint_read_record(inner, ack_entry, ack_plaintext_bytes)?;
                        inner.cipher.inspect_ack_finalizer_with_adoption(
                            &completion_receipt,
                            &request,
                            adoption_receipt,
                            &ack_record,
                        )?;
                    }
                }
            }
            Ok(GuardianCheckpointStageReplyV1::Acked {
                upload_id: shape.upload_id,
                completion_id: inspection.publication_id,
                checkpoint_id: shape.descriptor.checkpoint_id(),
                boundary_id: shape.descriptor.boundary_id(),
                total_bytes: shape.total_bytes,
            })
        })
    }

    /// Finalize one sealed-but-unadopted completion only under the opaque
    /// policy receipt issued by the durable retention transaction.  This path
    /// is intentionally not routed by the production worker until the catalog
    /// and retained-recovery-generation fences exist.
    #[allow(dead_code)]
    pub(crate) fn apply_expiry(
        &self,
        request: GuardianCheckpointStageRequestV1,
        expiry_receipt: GuardianCheckpointPolicyExpiryReceiptV1,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianCheckpointStageStoreError> {
        if request.kind() != GuardianCheckpointStageKindV1::Query {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        let shape = CheckpointStageRequestShape::from_request(&request)?;
        self.with_exclusive_directory(|inner| {
            let census = checkpoint_stage_census(inner)?;
            let inspection = checkpoint_inspect_upload(
                inner,
                &census,
                &shape,
                CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
            )?
            .ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
            if !inspection.seal_present
                || inspection.next_index != shape.total_chunks
                || inspection.committed_bytes != shape.total_bytes
            {
                return Err(GuardianCheckpointStageStoreError::OutOfOrder);
            }
            if inspection.ack_present {
                return Err(GuardianCheckpointStageStoreError::Conflict);
            }
            let ordered_chunk_set_identity = inspection
                .ordered_chunk_set_identity
                .ok_or(GuardianCheckpointStageStoreError::OutOfOrder)?;
            let payload =
                checkpoint_assemble_payload(inner, &census, &shape, inspection.publication_id)?;
            request.validate_staged_plaintext(payload.as_slice())?;
            let seal_request = GuardianCheckpointStageRequestV1::seal(
                shape.scope,
                shape.upload_id,
                shape.descriptor,
                shape.chunk_bytes,
            )?;
            let seal_entry = census
                .entries
                .iter()
                .find(|entry| {
                    entry.key == shape.key()
                        && entry.role
                            == (CheckpointStageFileRole::Seal {
                                publication_id: inspection.publication_id,
                            })
                })
                .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
            let (_, seal_record, _) =
                checkpoint_read_record(inner, seal_entry, GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)?;
            let completion_receipt = inner.cipher.inspect_durable_manifest_receipt(
                &shape.binding,
                seal_request,
                inspection.publication_id,
                inspection.candidate_identity,
                ordered_chunk_set_identity,
                &seal_record,
            )?;
            let expiry_path =
                checkpoint_expiry_path(inner, shape.key(), inspection.publication_id)?;
            let expiry_plaintext_bytes = u32::try_from(CHECKPOINT_STAGE_EXPIRY_PLAINTEXT_BYTES)
                .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
            if inspection.expiry_present {
                let expiry_entry = census
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.key == shape.key()
                            && entry.role
                                == (CheckpointStageFileRole::Expired {
                                    publication_id: inspection.publication_id,
                                })
                    })
                    .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                let (_, expiry_record, _) =
                    checkpoint_read_record(inner, expiry_entry, expiry_plaintext_bytes)?;
                inner.cipher.inspect_expiry_finalizer_with_policy(
                    &completion_receipt,
                    &request,
                    &expiry_receipt,
                    &expiry_record,
                )?;
            } else {
                let record_bytes =
                    checkpoint_record_bytes_for_plaintext(CHECKPOINT_STAGE_EXPIRY_PLAINTEXT_BYTES)?;
                checkpoint_stage_require_capacity(inner, &census, shape.key(), 1, record_bytes)?;
                match checkpoint_create_record_new(inner, &expiry_path)? {
                    CheckpointStageCreateOutcome::Created(file) => {
                        let record = inner.cipher.seal_expiry_finalizer(
                            &completion_receipt,
                            &request,
                            expiry_receipt,
                        )?;
                        checkpoint_write_created_record(inner, &expiry_path, file, &record)?;
                    }
                    CheckpointStageCreateOutcome::Existing => {
                        let refreshed = checkpoint_stage_census(inner)?;
                        let expiry_entry = refreshed
                            .entries
                            .iter()
                            .find(|entry| {
                                entry.key == shape.key()
                                    && entry.role
                                        == (CheckpointStageFileRole::Expired {
                                            publication_id: inspection.publication_id,
                                        })
                            })
                            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                        let (_, expiry_record, _) =
                            checkpoint_read_record(inner, expiry_entry, expiry_plaintext_bytes)?;
                        inner.cipher.inspect_expiry_finalizer_with_policy(
                            &completion_receipt,
                            &request,
                            &expiry_receipt,
                            &expiry_record,
                        )?;
                    }
                }
            }
            Ok(GuardianCheckpointStageReplyV1::Expired {
                upload_id: shape.upload_id,
                completion_id: inspection.publication_id,
                checkpoint_id: shape.descriptor.checkpoint_id(),
                boundary_id: shape.descriptor.boundary_id(),
                total_bytes: shape.total_bytes,
            })
        })
    }

    /// Consume protocol-authenticated live-lease Seal authority and independently
    /// bind it to the exact output-journal boundary recovered by this store.
    pub(crate) fn apply_runtime_seal(
        &self,
        permit: GuardianCheckpointRuntimeSealPermitV1,
        journal: &GuardianPaneOutputJournal,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianCheckpointStageStoreError> {
        let shape = CheckpointStageRequestShape::from_request(permit.request())?;
        if !matches!(shape.path_scope, CheckpointStagePathScope::Pane { .. }) {
            return Err(GuardianCheckpointStageStoreError::OriginAuthorityMismatch);
        }
        let (manifest_authority, request) =
            GuardianCheckpointValidatedManifestAuthorityV1::from_guardian_runtime_seal_permit(
                &shape.binding,
                permit,
            )?;
        self.apply_seal(
            request,
            GuardianCheckpointOriginAuthority::Record {
                journal,
                manifest_authority,
            },
        )
    }

    pub(crate) fn apply_seal(
        &self,
        request: GuardianCheckpointStageRequestV1,
        origin_authority: GuardianCheckpointOriginAuthority<'_>,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianCheckpointStageStoreError> {
        if request.kind() != GuardianCheckpointStageKindV1::Seal {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        let shape = CheckpointStageRequestShape::from_request(&request)?;
        self.with_exclusive_directory(|inner| {
            let census = checkpoint_stage_census(inner)?;
            let inspection = checkpoint_inspect_upload(
                inner,
                &census,
                &shape,
                CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
            )?
            .ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
            if inspection.next_index != shape.total_chunks
                || inspection.committed_bytes != shape.total_bytes
            {
                return Err(GuardianCheckpointStageStoreError::OutOfOrder);
            }
            let ordered_chunk_set_identity = inspection
                .ordered_chunk_set_identity
                .ok_or(GuardianCheckpointStageStoreError::OutOfOrder)?;
            let payload =
                checkpoint_assemble_payload(inner, &census, &shape, inspection.publication_id)?;
            request.validate_staged_plaintext(payload.as_slice())?;
            let manifest_authority = match origin_authority {
                GuardianCheckpointOriginAuthority::Record {
                    journal,
                    manifest_authority,
                } if matches!(shape.path_scope, CheckpointStagePathScope::Pane { .. }) => {
                    journal.validate_checkpoint_record_origin(&shape.canonical_descriptor)?;
                    manifest_authority
                }
                GuardianCheckpointOriginAuthority::Genesis { manifest_authority }
                    if matches!(shape.path_scope, CheckpointStagePathScope::Genesis { .. }) =>
                {
                    manifest_authority
                }
                GuardianCheckpointOriginAuthority::Record { .. }
                | GuardianCheckpointOriginAuthority::Genesis { .. } => {
                    return Err(GuardianCheckpointStageStoreError::OriginAuthorityMismatch);
                }
            };
            let capabilities = manifest_authority.bind_durable_stage_assembly(
                request,
                inspection.publication_id,
                inspection.candidate_identity,
                ordered_chunk_set_identity,
            )?;
            let (primary, retry) = capabilities.into_primary_and_retry();
            let seal_path = checkpoint_seal_path(inner, shape.key(), inspection.publication_id)?;
            if inspection.seal_present {
                let seal_entry = census
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.key == shape.key()
                            && entry.role
                                == (CheckpointStageFileRole::Seal {
                                    publication_id: inspection.publication_id,
                                })
                    })
                    .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                let (_, record, _) = checkpoint_read_record(
                    inner,
                    seal_entry,
                    GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES,
                )?;
                inner.cipher.retry_open_manifest(&retry, &record)?;
            } else {
                let record_bytes = checkpoint_record_bytes_for_plaintext(
                    usize::try_from(GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)
                        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
                )?;
                checkpoint_stage_require_capacity(inner, &census, shape.key(), 1, record_bytes)?;
                match checkpoint_create_record_new(inner, &seal_path)? {
                    CheckpointStageCreateOutcome::Created(file) => {
                        let record = inner.cipher.seal_manifest(primary)?;
                        checkpoint_write_created_record(inner, &seal_path, file, &record)?;
                    }
                    CheckpointStageCreateOutcome::Existing => {
                        let refreshed = checkpoint_stage_census(inner)?;
                        let seal_entry = refreshed
                            .entries
                            .iter()
                            .find(|entry| {
                                entry.key == shape.key()
                                    && entry.role
                                        == (CheckpointStageFileRole::Seal {
                                            publication_id: inspection.publication_id,
                                        })
                            })
                            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                        let (_, record, _) = checkpoint_read_record(
                            inner,
                            seal_entry,
                            GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES,
                        )?;
                        inner.cipher.retry_open_manifest(&retry, &record)?;
                    }
                }
            }
            Ok(GuardianCheckpointStageReplyV1::Sealed {
                upload_id: shape.upload_id,
                completion_id: inspection.publication_id,
                checkpoint_id: shape.descriptor.checkpoint_id(),
                boundary_id: shape.descriptor.boundary_id(),
                total_bytes: shape.total_bytes,
            })
        })
    }

    /// Publish an exact sealed Stage upload under the mutation-sequenced,
    /// non-forgeable Checkpoint adoption permit. This is an active production
    /// storage path; it does not enable checkpoint ACK or upgrade activation.
    #[allow(dead_code)]
    pub(crate) fn publish_sealed_catalog_member(
        &self,
        request: &GuardianCheckpointStageRequestV1,
        permit: GuardianCheckpointCatalogAdoptionPermitV1,
    ) -> Result<(), GuardianCheckpointStageStoreError> {
        let seed = permit.into_evidence_seed();
        self.with_exclusive_directory(|inner| {
            checkpoint_catalog_publish_sealed_stage(inner, request, seed).map(|_| ())
        })
    }

    /// Resolve the exact sealed Stage upload named by one authenticated
    /// Checkpoint mutation, then publish it under that mutation's single-use
    /// adoption authority.
    pub(crate) fn publish_checkpoint_catalog_adoption(
        &self,
        permit: GuardianCheckpointCatalogAdoptionPermitV1,
    ) -> Result<(), GuardianCheckpointStageStoreError> {
        let seed = permit.into_evidence_seed();
        let pane_id = seed.pane_id();
        let generation = seed.generation();
        let checkpoint_identity = seed.checkpoint_identity_digest();
        let boundary_identity = seed.output_boundary_identity_digest();
        self.with_exclusive_directory(|inner| {
            let census = checkpoint_stage_census(inner)?;
            let expected_scope = CheckpointStagePathScope::Pane {
                pane_id,
                generation,
            };
            let mut selected = None;
            for entry in census.entries.iter().filter(|entry| {
                entry.key.scope == expected_scope
                    && entry.role == CheckpointStageFileRole::Candidate
            }) {
                let (_, plaintext) = checkpoint_open_record(
                    inner,
                    entry,
                    u32::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES)
                        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
                )?;
                let begin = GuardianCheckpointStageRequestV1::decode(&plaintext)?;
                if begin.kind() != GuardianCheckpointStageKindV1::Begin {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                let shape = CheckpointStageRequestShape::from_request(&begin)?;
                if shape.key() != entry.key {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                if shape.descriptor.checkpoint_id().into_bytes() != checkpoint_identity
                    || shape.descriptor.boundary_id().into_bytes() != boundary_identity
                {
                    continue;
                }
                let inspection = checkpoint_inspect_upload(
                    inner,
                    &census,
                    &shape,
                    CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
                )?
                .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                if !inspection.seal_present
                    || inspection.ack_present
                    || inspection.expiry_present
                    || inspection.next_index != shape.total_chunks
                    || inspection.committed_bytes != shape.total_bytes
                {
                    return Err(GuardianCheckpointStageStoreError::OutOfOrder);
                }
                if selected.is_some() {
                    return Err(GuardianCheckpointStageStoreError::Conflict);
                }
                selected = Some(GuardianCheckpointStageRequestV1::seal(
                    shape.scope,
                    shape.upload_id,
                    shape.descriptor,
                    shape.chunk_bytes,
                )?);
            }
            let request = selected.ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
            checkpoint_catalog_publish_sealed_stage(inner, &request, seed).map(|_| ())
        })
    }

    /// Consume the exact reservation continuation produced when the nonclone
    /// Spawn permit was split into Genesis-seal authority, publish its already
    /// sealed upload, and return PTY-admission authority only after a candidate
    /// + marker directory sync and a complete catalog rescan.
    pub(crate) fn publish_genesis_catalog_admission(
        &self,
        reservation_identity: GuardianGenesisReservationIdentityV1,
    ) -> Result<GuardianPublishedGenesisAdmissionPermitV1, GuardianCheckpointStageStoreError> {
        let reservation = CheckpointCatalogGenesisReservationBinding::from(&reservation_identity);
        let catalog_candidate_checksum = self.with_exclusive_directory(|inner| {
            let census = checkpoint_stage_census(inner)?;
            let expected_scope = CheckpointStagePathScope::Genesis {
                spawn_effect_id: reservation.spawn_effect_id,
            };
            let expected_key = CheckpointStageUploadKey {
                scope: expected_scope,
                upload_id: reservation.upload_id,
            };
            let mut selected = None;
            for entry in census.entries.iter().filter(|entry| {
                entry.key == expected_key && entry.role == CheckpointStageFileRole::Candidate
            }) {
                let (_, plaintext) = checkpoint_open_record(
                    inner,
                    entry,
                    u32::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES)
                        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
                )?;
                let begin = GuardianCheckpointStageRequestV1::decode(&plaintext)?;
                if begin.kind() != GuardianCheckpointStageKindV1::Begin {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                let shape = CheckpointStageRequestShape::from_request(&begin)?;
                if shape.key() != entry.key {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                if shape.descriptor.capture_generation() != 1
                    || shape.descriptor.checkpoint_id().into_bytes()
                        != reservation.checkpoint_identity_digest
                    || shape.descriptor.boundary_id().into_bytes()
                        != reservation.boundary_identity_digest
                    || shape.descriptor.rows() != u32::from(reservation.rows)
                    || shape.descriptor.cols() != u32::from(reservation.cols)
                {
                    return Err(GuardianCheckpointStageStoreError::Conflict);
                }
                let inspection = checkpoint_inspect_upload(
                    inner,
                    &census,
                    &shape,
                    CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
                )?
                .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
                if !inspection.seal_present
                    || inspection.ack_present
                    || inspection.expiry_present
                    || inspection.next_index != shape.total_chunks
                    || inspection.committed_bytes != shape.total_bytes
                {
                    return Err(GuardianCheckpointStageStoreError::OutOfOrder);
                }
                if selected.is_some() {
                    return Err(GuardianCheckpointStageStoreError::Conflict);
                }
                selected = Some(GuardianCheckpointStageRequestV1::seal(
                    shape.scope,
                    shape.upload_id,
                    shape.descriptor,
                    shape.chunk_bytes,
                )?);
            }
            let request = selected.ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
            checkpoint_catalog_publish_genesis_stage(inner, &request, reservation)
                .map(|member| member.candidate_checksum)
        })?;
        Ok(GuardianPublishedGenesisAdmissionPermitV1 {
            reservation_identity,
            catalog_candidate_checksum,
        })
    }

    /// Select only a checksum-bound published member with current replay
    /// semantics. Directory entry timestamps are deliberately never read.
    #[allow(dead_code)]
    fn latest_compatible_catalog_member(
        &self,
        stage_scope: CheckpointStagePathScope,
        replay_semantics_id: [u8; 32],
    ) -> Result<Option<[u8; 32]>, GuardianCheckpointStageStoreError> {
        if replay_semantics_id == [0; 32] {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        self.with_exclusive_directory(|inner| {
            let scan = checkpoint_catalog_scan(
                inner,
                CheckpointCatalogScope::from_stage_scope(stage_scope),
            )?;
            Ok(scan
                .published
                .iter()
                .rev()
                .find(|member| {
                    member
                        .format
                        .authorizes_scope(member.metadata.identity.scope)
                        && member.metadata.replay_semantics_id == replay_semantics_id
                })
                .map(|member| member.metadata.checkpoint_id))
        })
    }

    /// Resolve one exact artifact identity from the published catalog only.
    #[allow(dead_code)]
    fn exact_catalog_member(
        &self,
        stage_scope: CheckpointStagePathScope,
        checkpoint_id: [u8; 32],
    ) -> Result<Option<[u8; 32]>, GuardianCheckpointStageStoreError> {
        if checkpoint_id == [0; 32] {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        self.with_exclusive_directory(|inner| {
            let scan = checkpoint_catalog_scan(
                inner,
                CheckpointCatalogScope::from_stage_scope(stage_scope),
            )?;
            Ok(scan
                .published
                .iter()
                .find(|member| {
                    member
                        .format
                        .authorizes_scope(member.metadata.identity.scope)
                        && member.metadata.checkpoint_id == checkpoint_id
                })
                .map(|member| member.metadata.checkpoint_id))
        })
    }

    fn with_exclusive_directory<T>(
        &self,
        operation: impl FnOnce(
            &GuardianCheckpointStageStoreInner,
        ) -> Result<T, GuardianCheckpointStageStoreError>,
    ) -> Result<T, GuardianCheckpointStageStoreError> {
        let _gate = self
            .inner
            .gate
            .lock()
            .map_err(|_| GuardianCheckpointStageStoreError::LockPoisoned)?;
        let mut directory_lock = CheckpointStageDirectoryLock::exclusive(&self.inner.directory)?;
        self.inner
            .persistence
            .validate(&self.inner.directory)
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
        let result = operation(&self.inner);
        let unlock = directory_lock.unlock();
        match (result, unlock) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => Err(GuardianCheckpointStageStoreError::io(
                "checkpoint-directory-unlock",
                error,
            )),
        }
    }
}

struct CheckpointStageDirectoryLock<'a> {
    directory: &'a File,
    locked: bool,
}

impl<'a> CheckpointStageDirectoryLock<'a> {
    fn exclusive(directory: &'a File) -> Result<Self, GuardianCheckpointStageStoreError> {
        checkpoint_stage_lock_directory(directory).map_err(|error| {
            GuardianCheckpointStageStoreError::io("checkpoint-directory-lock", error)
        })?;
        Ok(Self {
            directory,
            locked: true,
        })
    }

    fn unlock(&mut self) -> std::io::Result<()> {
        if self.locked {
            checkpoint_stage_unlock_directory(self.directory)?;
            self.locked = false;
        }
        Ok(())
    }
}

impl Drop for CheckpointStageDirectoryLock<'_> {
    fn drop(&mut self) {
        if self.locked {
            let _ = checkpoint_stage_unlock_directory(self.directory);
            self.locked = false;
        }
    }
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn checkpoint_stage_lock_directory(directory: &File) -> std::io::Result<()> {
    rustix::fs::flock(directory, rustix::fs::FlockOperation::LockExclusive)
        .map_err(std::io::Error::from)
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn checkpoint_stage_unlock_directory(directory: &File) -> std::io::Result<()> {
    rustix::fs::flock(directory, rustix::fs::FlockOperation::Unlock).map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn checkpoint_stage_lock_directory(_directory: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "safe guardian checkpoint locking is unsupported on this Unix target",
    ))
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn checkpoint_stage_unlock_directory(_directory: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "safe guardian checkpoint unlocking is unsupported on this Unix target",
    ))
}

fn checkpoint_stage_name_max(directory: &File) -> Result<usize, GuardianOutputError> {
    let observed = fpathconf(directory, PathconfVar::NAME_MAX)
        .map_err(|error| {
            GuardianOutputError::io("checkpoint-directory-name-max", std::io::Error::from(error))
        })?
        .ok_or(GuardianOutputError::FilesystemAuthority(
            "guardian checkpoint directory has no finite name bound",
        ))?;
    usize::try_from(observed).map_err(|_| {
        GuardianOutputError::FilesystemAuthority(
            "guardian checkpoint directory name bound is invalid",
        )
    })
}

fn checkpoint_stage_longest_name_bytes() -> usize {
    let scope = CheckpointStagePathScope::Pane {
        pane_id: Uuid::from_u128(u128::MAX),
        generation: u64::MAX,
    };
    format!(
        "{}.publication-{}.chunk-{:010}{CHECKPOINT_STAGE_FILE_SUFFIX}",
        scope.base_name(Uuid::from_u128(u128::MAX)),
        Uuid::from_u128(u128::MAX),
        GUARDIAN_MAX_CHECKPOINT_CHUNKS - 1,
    )
    .len()
}

fn checkpoint_catalog_base_name(identity: CheckpointCatalogIdentity) -> String {
    let scope = match identity.scope {
        CheckpointCatalogScope::Pane { pane_id } => format!("pane-{pane_id}"),
        CheckpointCatalogScope::Genesis { spawn_effect_id } => {
            format!("genesis-{spawn_effect_id}")
        }
    };
    format!(
        "{CHECKPOINT_CATALOG_FILE_PREFIX}{scope}.generation-{:020}.candidate-{}",
        identity.generation, identity.candidate_id
    )
}

fn checkpoint_catalog_path(
    inner: &GuardianCheckpointStageStoreInner,
    identity: CheckpointCatalogIdentity,
    role: CheckpointCatalogPathRole,
) -> Result<PathBuf, GuardianCheckpointStageStoreError> {
    let suffix = match role {
        CheckpointCatalogPathRole::Candidate => CHECKPOINT_CATALOG_CANDIDATE_SUFFIX,
        CheckpointCatalogPathRole::Marker => CHECKPOINT_CATALOG_MARKER_SUFFIX,
    };
    let name = format!("{}{suffix}", checkpoint_catalog_base_name(identity));
    if name.len() > inner.name_max {
        return Err(GuardianCheckpointStageStoreError::NameLimit);
    }
    Ok(inner.directory_path.join(name))
}

fn checkpoint_catalog_staging_path(
    inner: &GuardianCheckpointStageStoreInner,
    canonical_path: &Path,
) -> Result<PathBuf, GuardianCheckpointStageStoreError> {
    let canonical_name = output_child_name(&inner.directory_path, canonical_path)?;
    let mut name = canonical_name.to_os_string();
    name.push(CHECKPOINT_CATALOG_STAGING_SUFFIX);
    if name.len() > inner.name_max || !name.as_bytes().is_ascii() {
        return Err(GuardianCheckpointStageStoreError::NameLimit);
    }
    Ok(inner.directory_path.join(name))
}

fn checkpoint_catalog_longest_name_bytes() -> usize {
    let identity = CheckpointCatalogIdentity {
        scope: CheckpointCatalogScope::Genesis {
            spawn_effect_id: Uuid::from_u128(u128::MAX),
        },
        generation: u64::MAX,
        candidate_id: Uuid::from_u128(u128::MAX),
    };
    format!(
        "{}{}{}",
        checkpoint_catalog_base_name(identity),
        CHECKPOINT_CATALOG_CANDIDATE_SUFFIX,
        CHECKPOINT_CATALOG_STAGING_SUFFIX,
    )
    .len()
}

fn checkpoint_catalog_parse_path(
    name: &str,
) -> Option<(CheckpointCatalogIdentity, CheckpointCatalogPathKind)> {
    let (canonical_name, staged) = name
        .strip_suffix(CHECKPOINT_CATALOG_STAGING_SUFFIX)
        .map_or((name, false), |canonical| (canonical, true));
    let (body, role) =
        if let Some(body) = canonical_name.strip_suffix(CHECKPOINT_CATALOG_CANDIDATE_SUFFIX) {
            (body, CheckpointCatalogPathRole::Candidate)
        } else {
            (
                canonical_name.strip_suffix(CHECKPOINT_CATALOG_MARKER_SUFFIX)?,
                CheckpointCatalogPathRole::Marker,
            )
        };
    let body = body.strip_prefix(CHECKPOINT_CATALOG_FILE_PREFIX)?;
    let (scope_text, generation_and_candidate) = body.split_once(".generation-")?;
    let (generation_text, candidate_text) = generation_and_candidate.split_once(".candidate-")?;
    if generation_text.len() != 20 || !generation_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let generation = generation_text.parse::<u64>().ok()?;
    let candidate_id = candidate_text.parse::<Uuid>().ok()?;
    let scope = if let Some(pane_id) = scope_text.strip_prefix("pane-") {
        CheckpointCatalogScope::Pane {
            pane_id: pane_id.parse().ok()?,
        }
    } else {
        let spawn_effect_id = scope_text.strip_prefix("genesis-")?;
        CheckpointCatalogScope::Genesis {
            spawn_effect_id: spawn_effect_id.parse().ok()?,
        }
    };
    let identity = CheckpointCatalogIdentity {
        scope,
        generation,
        candidate_id,
    };
    let mut expected = match role {
        CheckpointCatalogPathRole::Candidate => format!(
            "{}{}",
            checkpoint_catalog_base_name(identity),
            CHECKPOINT_CATALOG_CANDIDATE_SUFFIX
        ),
        CheckpointCatalogPathRole::Marker => format!(
            "{}{}",
            checkpoint_catalog_base_name(identity),
            CHECKPOINT_CATALOG_MARKER_SUFFIX
        ),
    };
    let kind = if staged {
        expected.push_str(CHECKPOINT_CATALOG_STAGING_SUFFIX);
        CheckpointCatalogPathKind::Staging(role)
    } else {
        CheckpointCatalogPathKind::Canonical(role)
    };
    (name == expected && generation > 0 && !candidate_id.is_nil() && !scope.identity().is_nil())
        .then_some((identity, kind))
}

fn checkpoint_candidate_path(
    inner: &GuardianCheckpointStageStoreInner,
    key: CheckpointStageUploadKey,
) -> Result<PathBuf, GuardianCheckpointStageStoreError> {
    checkpoint_stage_path(
        inner,
        format!(
            "{}.candidate{CHECKPOINT_STAGE_FILE_SUFFIX}",
            key.base_name()
        ),
    )
}

fn checkpoint_chunk_path(
    inner: &GuardianCheckpointStageStoreInner,
    key: CheckpointStageUploadKey,
    publication_id: Uuid,
    index: u32,
) -> Result<PathBuf, GuardianCheckpointStageStoreError> {
    checkpoint_stage_path(
        inner,
        format!(
            "{}.publication-{publication_id}.chunk-{index:010}{CHECKPOINT_STAGE_FILE_SUFFIX}",
            key.base_name(),
        ),
    )
}

fn checkpoint_seal_path(
    inner: &GuardianCheckpointStageStoreInner,
    key: CheckpointStageUploadKey,
    publication_id: Uuid,
) -> Result<PathBuf, GuardianCheckpointStageStoreError> {
    checkpoint_stage_path(
        inner,
        format!(
            "{}.publication-{publication_id}.seal{CHECKPOINT_STAGE_FILE_SUFFIX}",
            key.base_name(),
        ),
    )
}

fn checkpoint_ack_path(
    inner: &GuardianCheckpointStageStoreInner,
    key: CheckpointStageUploadKey,
    publication_id: Uuid,
) -> Result<PathBuf, GuardianCheckpointStageStoreError> {
    checkpoint_stage_path(
        inner,
        format!(
            "{}.publication-{publication_id}.ack{CHECKPOINT_STAGE_FILE_SUFFIX}",
            key.base_name(),
        ),
    )
}

fn checkpoint_expiry_path(
    inner: &GuardianCheckpointStageStoreInner,
    key: CheckpointStageUploadKey,
    publication_id: Uuid,
) -> Result<PathBuf, GuardianCheckpointStageStoreError> {
    checkpoint_stage_path(
        inner,
        format!(
            "{}.publication-{publication_id}.expired{CHECKPOINT_STAGE_FILE_SUFFIX}",
            key.base_name(),
        ),
    )
}

fn checkpoint_stage_path(
    inner: &GuardianCheckpointStageStoreInner,
    name: String,
) -> Result<PathBuf, GuardianCheckpointStageStoreError> {
    if name.len() > inner.name_max || !name.as_bytes().is_ascii() {
        return Err(GuardianCheckpointStageStoreError::NameLimit);
    }
    Ok(inner.directory_path.join(name))
}

fn checkpoint_stage_census(
    inner: &GuardianCheckpointStageStoreInner,
) -> Result<CheckpointStageCensus, GuardianCheckpointStageStoreError> {
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    let mut entries = Vec::new();
    let mut uploads = BTreeSet::new();
    let mut semantic_names = BTreeSet::new();
    let mut total_bytes = 0_u64;
    for name in read_directory_names(&inner.directory)? {
        let raw = name.as_bytes();
        // Stage uploads and the immutable published catalog intentionally
        // share one pinned directory. The catalog owns the narrower
        // `checkpoint-catalog-` namespace and validates every such entry in
        // `checkpoint_catalog_scan`; do not misparse those files as malformed
        // Stage upload names on an exact adoption retry.
        if raw.starts_with(CHECKPOINT_CATALOG_FILE_PREFIX.as_bytes()) {
            continue;
        }
        if !raw.starts_with(CHECKPOINT_STAGE_FILE_PREFIX) {
            continue;
        }
        if raw.len() > inner.name_max {
            return Err(GuardianCheckpointStageStoreError::NameLimit);
        }
        let (key, role) = checkpoint_parse_stage_name(raw)?;
        if !semantic_names.insert((key, role)) {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        let path = inner.directory_path.join(&name);
        let file = open_private_file_at(&inner.directory, &inner.directory_path, &path, false)?;
        let metadata = file.metadata().map_err(|error| {
            GuardianCheckpointStageStoreError::io("checkpoint-census-metadata", error)
        })?;
        validate_private_file_metadata(&metadata, None)?;
        total_bytes = total_bytes
            .checked_add(metadata.len())
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        if entries.len() >= inner.policy.max_stage_files
            || total_bytes > inner.policy.max_stage_bytes
        {
            return Err(GuardianCheckpointStageStoreError::Capacity);
        }
        entries
            .try_reserve(1)
            .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
        entries.push(CheckpointStageCensusEntry {
            key,
            role,
            path,
            bytes: metadata.len(),
        });
        uploads.insert(key);
        if uploads.len() > inner.policy.max_retained_uploads {
            return Err(GuardianCheckpointStageStoreError::Capacity);
        }
    }
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    Ok(CheckpointStageCensus {
        total_files: entries.len(),
        entries,
        uploads,
        total_bytes,
    })
}

fn checkpoint_parse_stage_name(
    raw: &[u8],
) -> Result<(CheckpointStageUploadKey, CheckpointStageFileRole), GuardianCheckpointStageStoreError>
{
    if !raw.is_ascii() {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let name = std::str::from_utf8(raw).map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    let (scope, after_scope) = if let Some(rest) = name.strip_prefix("checkpoint-pane-") {
        let (pane, rest) = checkpoint_take_uuid(rest)?;
        let rest = rest
            .strip_prefix(".generation-")
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let generation_text = rest
            .get(..20)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let generation = generation_text
            .parse::<u64>()
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
        if generation == 0 || format!("{generation:020}") != generation_text {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        let rest = rest
            .get(20..)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        (
            CheckpointStagePathScope::Pane {
                pane_id: pane,
                generation,
            },
            rest,
        )
    } else if let Some(rest) = name.strip_prefix("checkpoint-genesis-") {
        let (spawn_effect_id, rest) = checkpoint_take_uuid(rest)?;
        (CheckpointStagePathScope::Genesis { spawn_effect_id }, rest)
    } else {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    };
    let after_upload = after_scope
        .strip_prefix(".upload-")
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    let (upload_id, role_text) = checkpoint_take_uuid(after_upload)?;
    let role = if role_text == ".candidate.ftgcp" {
        CheckpointStageFileRole::Candidate
    } else if let Some(rest) = role_text.strip_prefix(".publication-") {
        let (publication_id, rest) = checkpoint_take_uuid(rest)?;
        if rest == ".seal.ftgcp" {
            CheckpointStageFileRole::Seal { publication_id }
        } else if rest == ".ack.ftgcp" {
            CheckpointStageFileRole::Ack { publication_id }
        } else if rest == ".expired.ftgcp" {
            CheckpointStageFileRole::Expired { publication_id }
        } else {
            let index_text = rest
                .strip_prefix(".chunk-")
                .and_then(|rest| rest.strip_suffix(CHECKPOINT_STAGE_FILE_SUFFIX))
                .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
            if index_text.len() != 10 {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            let index = index_text
                .parse::<u32>()
                .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
            if index >= GUARDIAN_MAX_CHECKPOINT_CHUNKS || format!("{index:010}") != index_text {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            CheckpointStageFileRole::Chunk {
                publication_id,
                index,
            }
        }
    } else {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    };
    Ok((CheckpointStageUploadKey { scope, upload_id }, role))
}

fn checkpoint_take_uuid(value: &str) -> Result<(Uuid, &str), GuardianCheckpointStageStoreError> {
    let encoded = value
        .get(..36)
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    let identity =
        Uuid::parse_str(encoded).map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    if identity.is_nil() || identity.to_string() != encoded {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let remainder = value
        .get(36..)
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    Ok((identity, remainder))
}

fn checkpoint_stage_require_capacity(
    inner: &GuardianCheckpointStageStoreInner,
    census: &CheckpointStageCensus,
    key: CheckpointStageUploadKey,
    added_files: usize,
    added_bytes: u64,
) -> Result<(), GuardianCheckpointStageStoreError> {
    let retained_uploads = census
        .uploads
        .len()
        .checked_add(usize::from(!census.uploads.contains(&key)))
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    let total_files = census
        .total_files
        .checked_add(added_files)
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    let total_bytes = census
        .total_bytes
        .checked_add(added_bytes)
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    let upload_files = census
        .entries
        .iter()
        .filter(|entry| entry.key == key)
        .count()
        .checked_add(added_files)
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    let upload_bytes = census
        .entries
        .iter()
        .filter(|entry| entry.key == key)
        .try_fold(0_u64, |bytes, entry| bytes.checked_add(entry.bytes))
        .and_then(|bytes| bytes.checked_add(added_bytes))
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    if retained_uploads > inner.policy.max_retained_uploads
        || total_files > inner.policy.max_stage_files
        || total_bytes > inner.policy.max_stage_bytes
        || upload_files > CHECKPOINT_STAGE_MAX_FILES_PER_UPLOAD
        || upload_bytes > CHECKPOINT_STAGE_MAX_BYTES_PER_UPLOAD
    {
        return Err(GuardianCheckpointStageStoreError::Capacity);
    }
    Ok(())
}

fn checkpoint_record_bytes(
    record: &GuardianEncryptedCheckpointStageRecordV1,
) -> Result<u64, GuardianCheckpointStageStoreError> {
    let actual = u64::try_from(GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES)
        .ok()
        .and_then(|header| {
            u64::try_from(record.ciphertext_bytes())
                .ok()
                .and_then(|ciphertext| header.checked_add(ciphertext))
        })
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    let expected = u64::from(record.plaintext_bytes())
        .checked_add(CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES)
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    if actual != expected {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    Ok(actual)
}

fn checkpoint_record_bytes_for_plaintext(
    plaintext_bytes: usize,
) -> Result<u64, GuardianCheckpointStageStoreError> {
    u64::try_from(plaintext_bytes)
        .ok()
        .and_then(|bytes| bytes.checked_add(CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES))
        .ok_or(GuardianCheckpointStageStoreError::Capacity)
}

fn checkpoint_create_record_new(
    inner: &GuardianCheckpointStageStoreInner,
    path: &Path,
) -> Result<CheckpointStageCreateOutcome, GuardianCheckpointStageStoreError> {
    match create_private_file_new_at(&inner.directory, &inner.directory_path, path) {
        Ok(file) => Ok(CheckpointStageCreateOutcome::Created(file)),
        Err(GuardianOutputError::Io { source, .. })
            if source.kind() == ErrorKind::AlreadyExists =>
        {
            Ok(CheckpointStageCreateOutcome::Existing)
        }
        Err(error) => Err(error.into()),
    }
}

fn checkpoint_write_created_record(
    inner: &GuardianCheckpointStageStoreInner,
    path: &Path,
    mut file: File,
    record: &GuardianEncryptedCheckpointStageRecordV1,
) -> Result<(), GuardianCheckpointStageStoreError> {
    let expected_bytes = checkpoint_record_bytes(record)?;
    let header = record.fixed_header();
    file.write_all(&header).map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-record-header-write", error)
    })?;
    file.write_all(record.ciphertext()).map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-record-ciphertext-write", error)
    })?;
    file.sync_all()
        .map_err(|error| GuardianCheckpointStageStoreError::io("checkpoint-record-sync", error))?;
    let metadata = file.metadata().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-record-metadata", error)
    })?;
    validate_private_file_metadata(&metadata, Some(expected_bytes))?;
    let identity = FileIdentity::capture(&metadata, Some(expected_bytes));
    inner.directory.sync_all().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-directory-sync", error)
    })?;
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    validate_file_identity_at(&inner.directory, &inner.directory_path, path, identity)?;
    checkpoint_remember_durable_record(inner, identity)?;
    Ok(())
}

fn checkpoint_remember_durable_record(
    inner: &GuardianCheckpointStageStoreInner,
    identity: FileIdentity,
) -> Result<(), GuardianCheckpointStageStoreError> {
    let mut durable_records = inner
        .durable_records
        .lock()
        .map_err(|_| GuardianCheckpointStageStoreError::LockPoisoned)?;
    if durable_records.contains(&identity) {
        return Ok(());
    }
    if durable_records.len() >= inner.policy.max_stage_files {
        return Err(GuardianCheckpointStageStoreError::Capacity);
    }
    durable_records
        .try_reserve_exact(1)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    durable_records.push(identity);
    Ok(())
}

fn checkpoint_read_record(
    inner: &GuardianCheckpointStageStoreInner,
    entry: &CheckpointStageCensusEntry,
    max_plaintext_bytes: u32,
) -> Result<
    (
        GuardianCheckpointStageRecordContextV1,
        GuardianEncryptedCheckpointStageRecordV1,
        FileIdentity,
    ),
    GuardianCheckpointStageStoreError,
> {
    if max_plaintext_bytes == 0
        || max_plaintext_bytes > GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let maximum_file_bytes = u64::from(max_plaintext_bytes)
        .checked_add(CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES)
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    if entry.bytes == 0 || entry.bytes > maximum_file_bytes {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let mut file =
        open_private_file_at(&inner.directory, &inner.directory_path, &entry.path, false)?;
    let metadata_before = file.metadata().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-record-open-metadata", error)
    })?;
    validate_private_file_metadata(&metadata_before, Some(entry.bytes))?;
    let identity = FileIdentity::capture(&metadata_before, Some(entry.bytes));
    let mut header = [0_u8; GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES];
    file.read_exact(&mut header).map_err(|error| {
        if error.kind() == ErrorKind::UnexpectedEof {
            GuardianCheckpointStageStoreError::Poisoned
        } else {
            GuardianCheckpointStageStoreError::io("checkpoint-record-header-read", error)
        }
    })?;
    let ciphertext_bytes = GuardianEncryptedCheckpointStageRecordV1::persisted_ciphertext_bytes(
        &header,
        max_plaintext_bytes,
    )
    .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    let exact_file_bytes = u64::try_from(GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES)
        .ok()
        .and_then(|header_bytes| {
            u64::try_from(ciphertext_bytes)
                .ok()
                .and_then(|body_bytes| header_bytes.checked_add(body_bytes))
        })
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    if exact_file_bytes != entry.bytes {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let mut ciphertext = Vec::new();
    ciphertext
        .try_reserve_exact(ciphertext_bytes)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    ciphertext.resize(ciphertext_bytes, 0);
    file.read_exact(&mut ciphertext).map_err(|error| {
        if error.kind() == ErrorKind::UnexpectedEof {
            GuardianCheckpointStageStoreError::Poisoned
        } else {
            GuardianCheckpointStageStoreError::io("checkpoint-record-ciphertext-read", error)
        }
    })?;
    let metadata_after = file.metadata().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-record-final-metadata", error)
    })?;
    if !identity.matches(&metadata_after) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let durability_known = inner
        .durable_records
        .lock()
        .map_err(|_| GuardianCheckpointStageStoreError::LockPoisoned)?
        .contains(&identity);
    if !durability_known {
        // An O_EXCL creator can lose its reply after the data sync but before
        // the directory sync, or a cooperating process can encounter a prior
        // sync error. Re-synchronize each identity once per store incarnation
        // before it can produce Ready/Progress/Sealed. The bounded cache avoids
        // turning an ordered 1,024-chunk upload into quadratic fsync traffic.
        file.sync_all().map_err(|error| {
            GuardianCheckpointStageStoreError::io("checkpoint-record-retry-sync", error)
        })?;
        inner.directory.sync_all().map_err(|error| {
            GuardianCheckpointStageStoreError::io("checkpoint-record-retry-directory-sync", error)
        })?;
        inner
            .persistence
            .validate(&inner.directory)
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
        validate_file_identity_at(
            &inner.directory,
            &inner.directory_path,
            &entry.path,
            identity,
        )?;
        checkpoint_remember_durable_record(inner, identity)?;
    }
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &entry.path,
        identity,
    )?;
    let record = GuardianEncryptedCheckpointStageRecordV1::from_persisted(
        &header,
        ciphertext,
        max_plaintext_bytes,
    )
    .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    let context = record.context();
    checkpoint_validate_context_path(entry, &context)?;
    Ok((context, record, identity))
}

fn checkpoint_open_record(
    inner: &GuardianCheckpointStageStoreInner,
    entry: &CheckpointStageCensusEntry,
    max_plaintext_bytes: u32,
) -> Result<
    (GuardianCheckpointStageRecordContextV1, Zeroizing<Vec<u8>>),
    GuardianCheckpointStageStoreError,
> {
    let (context, record, identity) = checkpoint_read_record(inner, entry, max_plaintext_bytes)?;
    let plaintext = inner
        .cipher
        .open(&context, &record, max_plaintext_bytes)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &entry.path,
        identity,
    )?;
    Ok((context, plaintext))
}

fn checkpoint_validate_context_path(
    entry: &CheckpointStageCensusEntry,
    context: &GuardianCheckpointStageRecordContextV1,
) -> Result<(), GuardianCheckpointStageStoreError> {
    if context.scope()
        != entry
            .key
            .scope
            .stage_scope()
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?
        || context.upload_id() != entry.key.upload_id
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let matches = match entry.role {
        CheckpointStageFileRole::Candidate => {
            context.kind() == GuardianCheckpointStageRecordKindV1::CandidateMetadata
                && context.chunk_position().is_none()
        }
        CheckpointStageFileRole::Chunk {
            publication_id,
            index,
        } => {
            context.kind() == GuardianCheckpointStageRecordKindV1::Chunk
                && context.publication_id() == publication_id
                && context
                    .chunk_position()
                    .is_some_and(|(observed, _)| observed == index)
        }
        CheckpointStageFileRole::Seal { publication_id } => {
            context.kind() == GuardianCheckpointStageRecordKindV1::SealManifest
                && context.publication_id() == publication_id
                && context.chunk_position().is_none()
        }
        CheckpointStageFileRole::Ack { publication_id } => {
            context.kind() == GuardianCheckpointStageRecordKindV1::Finalizer
                && context.publication_id() == publication_id
                && context.chunk_position().is_none()
        }
        CheckpointStageFileRole::Expired { publication_id } => {
            context.kind() == GuardianCheckpointStageRecordKindV1::Finalizer
                && context.publication_id() == publication_id
                && context.chunk_position().is_none()
        }
    };
    if matches {
        Ok(())
    } else {
        Err(GuardianCheckpointStageStoreError::Poisoned)
    }
}

fn checkpoint_context_public_identity_matches(
    observed: &GuardianCheckpointStageRecordContextV1,
    expected: &GuardianCheckpointStageRecordContextV1,
) -> bool {
    observed.kind() == expected.kind()
        && observed.scope() == expected.scope()
        && observed.upload_id() == expected.upload_id()
        && checkpoint_bytes_match(
            &observed.boundary_identity_digest(),
            &expected.boundary_identity_digest(),
        )
        && checkpoint_bytes_match(
            &observed.checkpoint_identity_digest(),
            &expected.checkpoint_identity_digest(),
        )
        && observed.publication_id() == expected.publication_id()
        && observed.chunk_position() == expected.chunk_position()
        && observed.plaintext_bytes() == expected.plaintext_bytes()
}

fn checkpoint_inspect_upload(
    inner: &GuardianCheckpointStageStoreInner,
    census: &CheckpointStageCensus,
    shape: &CheckpointStageRequestShape,
    seal_inspection: CheckpointStageSealInspection,
) -> Result<Option<CheckpointStageUploadInspection>, GuardianCheckpointStageStoreError> {
    let mut candidate = None;
    let mut chunks = BTreeMap::new();
    let mut seal = None;
    let mut ack = None;
    let mut expiry = None;
    for entry in census
        .entries
        .iter()
        .filter(|entry| entry.key == shape.key())
    {
        match entry.role {
            CheckpointStageFileRole::Candidate => {
                if candidate.replace(entry).is_some() {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
            }
            CheckpointStageFileRole::Chunk { index, .. } => {
                if chunks.insert(index, entry).is_some() {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
            }
            CheckpointStageFileRole::Seal { .. } => {
                if seal.replace(entry).is_some() {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
            }
            CheckpointStageFileRole::Ack { .. } => {
                if ack.replace(entry).is_some() {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
            }
            CheckpointStageFileRole::Expired { .. } => {
                if expiry.replace(entry).is_some() {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
            }
        }
    }
    let Some(candidate) = candidate else {
        return if chunks.is_empty() && seal.is_none() && ack.is_none() && expiry.is_none() {
            Ok(None)
        } else {
            Err(GuardianCheckpointStageStoreError::Poisoned)
        };
    };
    let seal_present = seal.is_some();
    let ack_present = ack.is_some();
    let expiry_present = expiry.is_some();
    if (ack_present && expiry_present) || ((ack_present || expiry_present) && !seal_present) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    if seal_present && matches!(seal_inspection, CheckpointStageSealInspection::Reject) {
        // V3 Seal records can be authenticated only with the exact primary or
        // retry operation capability. Production cannot mint the independent
        // assembled-stage witness yet, so no Seal can be adopted here and the
        // deterministic upload stays quarantined for a fresh upload identity.
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let begin_payload = shape.begin_payload()?;
    let (candidate_context, candidate_plaintext) = checkpoint_open_record(
        inner,
        candidate,
        u32::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES)
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
    )?;
    if !checkpoint_bytes_match(candidate_plaintext.as_slice(), begin_payload.as_slice()) {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let publication_id = candidate_context.publication_id();
    let candidate_identity =
        GuardianCheckpointCandidateIdentityV1::from_canonical_begin_plaintext(&begin_payload)?;
    let expected_candidate_intent = GuardianCheckpointStageSealIntentV1::candidate_metadata(
        &shape.binding,
        shape.upload_id,
        publication_id,
        begin_payload,
    )?;
    let expected_candidate = expected_candidate_intent.context();
    if !checkpoint_context_public_identity_matches(&candidate_context, &expected_candidate) {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let mut ordered_chunk_set = GuardianCheckpointOrderedChunkSetBuilderV1::new(
        shape.total_bytes,
        shape.chunk_bytes,
        shape.total_chunks,
    )?;
    let mut committed_bytes = 0_u64;
    for (expected_index, (index, entry)) in chunks.iter().enumerate() {
        let expected_index = u32::try_from(expected_index)
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
        if *index != expected_index || *index >= shape.total_chunks {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        let publication = match entry.role {
            CheckpointStageFileRole::Chunk { publication_id, .. } => publication_id,
            CheckpointStageFileRole::Candidate
            | CheckpointStageFileRole::Seal { .. }
            | CheckpointStageFileRole::Ack { .. }
            | CheckpointStageFileRole::Expired { .. } => {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
        };
        if publication != publication_id {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        let offset = u64::from(*index)
            .checked_mul(u64::from(shape.chunk_bytes))
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        let remaining = shape
            .total_bytes
            .checked_sub(offset)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let expected_bytes = remaining.min(u64::from(shape.chunk_bytes));
        let expected_bytes_u32 = u32::try_from(expected_bytes)
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
        let (context, plaintext) = checkpoint_open_record(inner, entry, expected_bytes_u32)?;
        let plaintext_bytes = u64::try_from(plaintext.len())
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
        ordered_chunk_set.push_authenticated_chunk(*index, offset, &plaintext)?;
        let expected_intent = GuardianCheckpointStageSealIntentV1::chunk(
            &shape.binding,
            shape.upload_id,
            publication_id,
            *index,
            offset,
            plaintext,
        )?;
        let expected_context = expected_intent.context();
        if !checkpoint_context_public_identity_matches(&context, &expected_context)
            || plaintext_bytes != expected_bytes
        {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        committed_bytes = committed_bytes
            .checked_add(expected_bytes)
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    }
    let next_index =
        u32::try_from(chunks.len()).map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
    let ordered_chunk_set_identity =
        if next_index == shape.total_chunks && committed_bytes == shape.total_bytes {
            Some(ordered_chunk_set.finish()?)
        } else {
            None
        };
    Ok(Some(CheckpointStageUploadInspection {
        publication_id,
        next_index,
        committed_bytes,
        seal_present,
        ack_present,
        expiry_present,
        candidate_identity,
        ordered_chunk_set_identity,
    }))
}

fn checkpoint_validate_exact_chunk_retry(
    inner: &GuardianCheckpointStageStoreInner,
    census: &CheckpointStageCensus,
    shape: &CheckpointStageRequestShape,
    publication_id: Uuid,
    index: u32,
    offset: u64,
    expected_plaintext: Zeroizing<Vec<u8>>,
) -> Result<(), GuardianCheckpointStageStoreError> {
    let mut matching = census.entries.iter().filter(|entry| {
        entry.key == shape.key()
            && entry.role
                == CheckpointStageFileRole::Chunk {
                    publication_id,
                    index,
                }
    });
    let entry = matching
        .next()
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    if matching.next().is_some() {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let max_plaintext_bytes = u32::try_from(expected_plaintext.len())
        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
    let (context, observed_plaintext) = checkpoint_open_record(inner, entry, max_plaintext_bytes)?;
    if !checkpoint_bytes_match(observed_plaintext.as_slice(), expected_plaintext.as_slice()) {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let expected_intent = GuardianCheckpointStageSealIntentV1::chunk(
        &shape.binding,
        shape.upload_id,
        publication_id,
        index,
        offset,
        expected_plaintext,
    )?;
    if !checkpoint_context_public_identity_matches(&context, &expected_intent.context()) {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    Ok(())
}

fn checkpoint_assemble_payload(
    inner: &GuardianCheckpointStageStoreInner,
    census: &CheckpointStageCensus,
    shape: &CheckpointStageRequestShape,
    publication_id: Uuid,
) -> Result<Zeroizing<Vec<u8>>, GuardianCheckpointStageStoreError> {
    let payload_bytes = usize::try_from(shape.total_bytes)
        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
    let mut payload = Zeroizing::new(Vec::new());
    payload
        .try_reserve_exact(payload_bytes)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    for index in 0..shape.total_chunks {
        let entry = census
            .entries
            .iter()
            .find(|entry| {
                entry.key == shape.key()
                    && entry.role
                        == CheckpointStageFileRole::Chunk {
                            publication_id,
                            index,
                        }
            })
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let offset = u64::from(index)
            .checked_mul(u64::from(shape.chunk_bytes))
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        let remaining = shape
            .total_bytes
            .checked_sub(offset)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let expected_bytes = remaining.min(u64::from(shape.chunk_bytes));
        let expected_bytes_u32 = u32::try_from(expected_bytes)
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
        let (context, chunk) = checkpoint_open_record(inner, entry, expected_bytes_u32)?;
        if u64::try_from(chunk.len()).ok() != Some(expected_bytes) {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        payload.extend_from_slice(chunk.as_slice());
        let expected_intent = GuardianCheckpointStageSealIntentV1::chunk(
            &shape.binding,
            shape.upload_id,
            publication_id,
            index,
            offset,
            chunk,
        )?;
        if !checkpoint_context_public_identity_matches(&context, &expected_intent.context()) {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
    }
    if payload.len() != payload_bytes {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    Ok(payload)
}

fn checkpoint_chunk_progress(
    shape: &CheckpointStageRequestShape,
    index: u32,
) -> Result<GuardianCheckpointStageReplyV1, GuardianCheckpointStageStoreError> {
    let next_index = index
        .checked_add(1)
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    let committed_bytes = u64::from(next_index)
        .checked_mul(u64::from(shape.chunk_bytes))
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?
        .min(shape.total_bytes);
    Ok(GuardianCheckpointStageReplyV1::Progress {
        upload_id: shape.upload_id,
        next_index,
        committed_bytes,
    })
}

fn checkpoint_bytes_match(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (*left ^ *right)
        })
        == 0
}

#[cfg(test)]
fn checkpoint_zeroizing_sha256_digest(bytes: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut digest = Zeroizing::new([0_u8; 32]);
    // Finalize directly into the zeroizing owner rather than producing a raw
    // array temporary between the SHA-256 state and its lifetime wrapper.
    let output: &mut sha2::digest::Output<Sha256> = (&mut *digest).into();
    Sha256::new_with_prefix(bytes).finalize_into(output);
    digest
}

/// Descriptor-pinned encrypted input WAL owned by the live-input worker while
/// one transaction is in flight.
pub struct GuardianPaneInputJournal {
    journal: GuardianInputJournal,
    directory: File,
    directory_path: PathBuf,
    path: PathBuf,
    file_identity: FileIdentity,
    persistence: Arc<PersistentOutputAuthority>,
}

pub type GuardianPaneInputTransaction = GuardianInputTransaction;

pub enum GuardianPaneInputTransactionError {
    Protocol(GuardianProtocolError),
    JournalBeforeWrite,
    AuthorityBeforeWrite,
    OutcomeIndeterminate,
    AcceptedJournalUnavailable,
    AcceptedAuthorityUnavailable,
    AcceptedProtocolUnavailable,
}

pub enum GuardianPaneInputCompletionError {
    DispositionIndeterminate,
    Journal,
    Authority,
    Protocol,
}

impl GuardianPaneInputJournal {
    fn validate_path_authority(&self) -> Result<(), GuardianOutputError> {
        self.persistence.validate(&self.directory).map_err(|_| {
            GuardianOutputError::FilesystemAuthority("guardian input persistence authority changed")
        })?;
        validate_file_identity_at(
            &self.directory,
            &self.directory_path,
            &self.path,
            FileIdentity {
                expected_len: Some(self.journal.committed_bytes()),
                ..self.file_identity
            },
        )
    }

    /// Worker-side primitive that durably admits one exact input and yields
    /// write authority only for the newly synchronized `AcceptedNotDurable`
    /// transition.
    pub(crate) fn begin_transaction(
        &mut self,
        protocol: &mut GuardianProtocolState,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianPaneInputTransaction, GuardianPaneInputTransactionError> {
        if self.validate_path_authority().is_err() {
            return match replay_guardian_input_without_writer(protocol, request) {
                Ok(_) => Err(GuardianPaneInputTransactionError::AcceptedAuthorityUnavailable),
                Err(GuardianEffectTransactionError::Protocol(error)) => {
                    Err(GuardianPaneInputTransactionError::Protocol(error))
                }
                Err(GuardianEffectTransactionError::Effect(())) => {
                    Err(GuardianPaneInputTransactionError::AuthorityBeforeWrite)
                }
                Err(GuardianEffectTransactionError::OutcomeIndeterminate(_)) => {
                    Err(GuardianPaneInputTransactionError::OutcomeIndeterminate)
                }
            };
        }
        let transaction = begin_guardian_input_transaction(protocol, &mut self.journal, request);
        match transaction {
            Ok(transaction) => {
                self.validate_path_authority()
                    .map_err(|_| GuardianPaneInputTransactionError::AcceptedAuthorityUnavailable)?;
                Ok(transaction)
            }
            Err(GuardianInputTransactionError::Protocol(error)) => {
                Err(GuardianPaneInputTransactionError::Protocol(error))
            }
            Err(GuardianInputTransactionError::JournalBeforeWrite(_)) => {
                Err(GuardianPaneInputTransactionError::JournalBeforeWrite)
            }
            Err(GuardianInputTransactionError::OutcomeIndeterminate(_)) => {
                Err(GuardianPaneInputTransactionError::OutcomeIndeterminate)
            }
            Err(GuardianInputTransactionError::AcceptedJournalUnavailable {
                accepted_reply: _,
                error: _,
            }) => {
                self.validate_path_authority()
                    .map_err(|_| GuardianPaneInputTransactionError::AcceptedAuthorityUnavailable)?;
                Err(GuardianPaneInputTransactionError::AcceptedJournalUnavailable)
            }
            Err(GuardianInputTransactionError::AcceptedProtocolUnavailable {
                accepted_reply: _,
                error: _,
            }) => {
                self.validate_path_authority()
                    .map_err(|_| GuardianPaneInputTransactionError::AcceptedAuthorityUnavailable)?;
                Err(GuardianPaneInputTransactionError::AcceptedProtocolUnavailable)
            }
        }
    }

    /// Synchronize the exact one-write outcome before completing protocol state.
    pub(crate) fn complete_write(
        &mut self,
        protocol: &mut GuardianProtocolState,
        outcome: GuardianInputWriteOutcome,
    ) -> Result<GuardianReply, GuardianPaneInputCompletionError> {
        self.validate_path_authority()
            .map_err(|_| GuardianPaneInputCompletionError::Authority)?;
        let completion = commit_guardian_input_outcome(&mut self.journal, outcome).map_err(
            |error| match error {
                GuardianInputCompletionError::DispositionIndeterminate => {
                    GuardianPaneInputCompletionError::DispositionIndeterminate
                }
                GuardianInputCompletionError::Journal(_) => {
                    GuardianPaneInputCompletionError::Journal
                }
                GuardianInputCompletionError::StateInvariant => {
                    GuardianPaneInputCompletionError::Protocol
                }
            },
        )?;
        self.validate_path_authority()
            .map_err(|_| GuardianPaneInputCompletionError::Authority)?;
        completion
            .reconcile_protocol(protocol)
            .map_err(|_| GuardianPaneInputCompletionError::Protocol)
    }
}

struct OutputJob {
    pane_id: Uuid,
    journal: GuardianPaneOutputJournal,
    payload: Zeroizing<Vec<u8>>,
}

#[derive(Debug, Error)]
enum OutputCommitError {
    #[error("guardian output persistence authority changed")]
    PersistenceAuthority,
    #[error("guardian output journal append failed")]
    Journal(#[from] GuardianOutputJournalError),
    #[error("guardian output journal lock was poisoned")]
    JournalLockPoisoned,
    #[error("guardian output immutable segment manager failed")]
    SegmentManager,
    #[error("guardian output bounded segment capacity is exhausted")]
    Capacity,
    #[error("guardian output worker queue accounting is inconsistent")]
    QueueInvariant,
}

pub struct GuardianOutputCompletion {
    pub(crate) pane_id: Uuid,
    pub(crate) payload_bytes: usize,
    pub(crate) result: Result<GuardianOutputAppendReceipt, GuardianOutputCommitFailure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianOutputCommitFailure;

pub enum GuardianOutputCompletionState {
    Ready(GuardianOutputCompletion),
    Empty,
    Disconnected,
}

pub enum GuardianOutputSubmitError {
    Saturated(Zeroizing<Vec<u8>>),
    Unavailable(Zeroizing<Vec<u8>>),
}

struct OutputQueueState {
    jobs: VecDeque<OutputJob>,
    outstanding: usize,
    max_outstanding: usize,
    shutdown: bool,
}

struct OutputQueue {
    state: Mutex<OutputQueueState>,
    ready: Condvar,
}

enum OutputQueuePushError {
    Saturated(OutputJob),
    Shutdown(OutputJob),
}

impl OutputQueue {
    fn new(max_outstanding: usize) -> Result<Self, GuardianOutputError> {
        if max_outstanding == 0 {
            return Err(GuardianOutputError::Allocation);
        }
        let mut jobs = VecDeque::new();
        jobs.try_reserve_exact(max_outstanding)
            .map_err(|_| GuardianOutputError::Allocation)?;
        Ok(Self {
            state: Mutex::new(OutputQueueState {
                jobs,
                outstanding: 0,
                max_outstanding,
                shutdown: false,
            }),
            ready: Condvar::new(),
        })
    }

    fn try_push(&self, job: OutputJob) -> Result<(), OutputQueuePushError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.shutdown {
            return Err(OutputQueuePushError::Shutdown(job));
        }
        if state.outstanding >= state.max_outstanding {
            return Err(OutputQueuePushError::Saturated(job));
        }
        state.outstanding += 1;
        state.jobs.push_back(job);
        self.ready.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<OutputJob> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if state.shutdown {
                return None;
            }
            if let Some(job) = state.jobs.pop_front() {
                return Some(job);
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    fn complete_one(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.outstanding == 0 {
            state.shutdown = true;
            self.ready.notify_all();
            return false;
        }
        state.outstanding -= 1;
        true
    }

    fn available_slots(&self) -> usize {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.shutdown {
            0
        } else {
            state.max_outstanding.saturating_sub(state.outstanding)
        }
    }

    fn shutdown(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.shutdown = true;
        while let Some(mut job) = state.jobs.pop_front() {
            job.payload.zeroize();
            state.outstanding = state.outstanding.saturating_sub(1);
        }
        self.ready.notify_all();
    }
}

/// Fixed-size worker pool plus secure per-pane journal factory.
pub struct GuardianOutputPipeline {
    directory: File,
    directory_path: PathBuf,
    cipher: GuardianOutputCipher,
    checkpoint_store: GuardianCheckpointStageStore,
    policy: OutputSegmentPolicy,
    persistence: Arc<PersistentOutputAuthority>,
    queue: Arc<OutputQueue>,
    completions: Option<Receiver<GuardianOutputCompletion>>,
    workers: Vec<JoinHandle<()>>,
    _completion_waker: Arc<Waker>,
}

impl GuardianOutputPipeline {
    pub(crate) fn open(
        token_path: &Path,
        max_panes: usize,
        completion_waker: Arc<Waker>,
    ) -> Result<Self, GuardianOutputError> {
        Self::open_with_policy(
            token_path,
            max_panes,
            completion_waker,
            OutputSegmentPolicy::production(),
        )
    }

    fn open_with_policy(
        token_path: &Path,
        max_panes: usize,
        completion_waker: Arc<Waker>,
        policy: OutputSegmentPolicy,
    ) -> Result<Self, GuardianOutputError> {
        let policy = policy.validate()?;
        let (directory, directory_path, parent_identity, directory_identity) =
            open_output_directory(token_path)?;
        let (cipher, key_path, key_identity) =
            load_or_create_output_key(&directory, &directory_path)?;
        let parent_path = directory_path
            .parent()
            .ok_or(GuardianOutputError::InvalidPath)?
            .to_path_buf();
        let persistence = Arc::new(PersistentOutputAuthority {
            parent_path,
            parent_identity,
            directory_path: directory_path.clone(),
            directory_identity,
            key_path,
            key_identity,
        });
        let checkpoint_store = GuardianCheckpointStageStore::open(
            &directory,
            &directory_path,
            &cipher,
            Arc::clone(&persistence),
            GuardianCheckpointStagePolicy::production(),
        )?;
        let max_outstanding = max_panes.clamp(1, OUTPUT_MAX_IN_FLIGHT);
        let queue = Arc::new(OutputQueue::new(max_outstanding)?);
        let (completion_tx, completions) = sync_channel(max_outstanding);
        let worker_count = OUTPUT_WORKER_THREADS.min(max_outstanding);
        let mut workers = Vec::new();
        workers
            .try_reserve_exact(worker_count)
            .map_err(|_| GuardianOutputError::Allocation)?;
        for index in 0..worker_count {
            let worker_queue = Arc::clone(&queue);
            let worker_completions = completion_tx.clone();
            let worker_waker = Arc::clone(&completion_waker);
            let spawn = thread::Builder::new()
                .name(format!("ft-guardian-output-{index}"))
                .spawn(move || {
                    output_worker(&worker_queue, &worker_completions, &worker_waker);
                });
            match spawn {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    queue.shutdown();
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(GuardianOutputError::io("output-worker-spawn", error));
                }
            }
        }
        drop(completion_tx);

        Ok(Self {
            directory,
            directory_path,
            cipher,
            checkpoint_store,
            policy,
            persistence,
            queue,
            completions: Some(completions),
            workers,
            _completion_waker: completion_waker,
        })
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn checkpoint_stage_store(&self) -> GuardianCheckpointStageStore {
        self.checkpoint_store.clone()
    }

    pub(crate) fn prepare_pane(
        &self,
        guardian_incarnation: Uuid,
        pane_id: Uuid,
    ) -> Result<GuardianPaneOutputJournal, GuardianOutputError> {
        self.persistence.validate(&self.directory).map_err(|_| {
            GuardianOutputError::FilesystemAuthority(
                "guardian output persistence authority changed",
            )
        })?;
        let authority = open_or_create_pane_segment_manager(
            &self.directory,
            &self.directory_path,
            Arc::clone(&self.persistence),
            self.cipher.clone(),
            self.policy,
            guardian_incarnation,
            pane_id,
        )?;
        // `prepare_pane` is a spawn-preparation seam, not restart recovery.
        // Reopening one empty, fully published chain is safe after PTY/mio
        // setup failed. Any prior raw output means the child disposition is
        // ambiguous and must never be attached to a replacement PTY here.
        if authority.total_records != 0
            || authority.segments.len() != 1
            || authority.physical_segment_files != 1
            || authority.manifest_history.len() != 1
            || authority.relevant_files != 3
        {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output spawn retry found an ambiguous nonempty chain",
            ));
        }
        pane_journal_handle(authority)
    }

    /// Prepare the empty encrypted WAL transferred into a newly spawned pane.
    ///
    /// This is not restart recovery: any existing journal, including a valid
    /// header-only prefix, makes replacement-child creation ambiguous and is
    /// withheld until a durable anti-rollback high-water authority exists.
    pub(crate) fn prepare_input(
        &self,
        guardian_incarnation: Uuid,
        pane_id: Uuid,
    ) -> Result<GuardianPaneInputJournal, GuardianOutputError> {
        self.persistence.validate(&self.directory).map_err(|_| {
            GuardianOutputError::FilesystemAuthority("guardian input persistence authority changed")
        })?;
        let directory = self
            .directory
            .try_clone()
            .map_err(|error| GuardianOutputError::io("input-directory-clone", error))?;
        let path = input_journal_path(&self.directory_path, guardian_incarnation, pane_id);
        let (file, created) =
            match open_private_file_at(&directory, &self.directory_path, &path, false) {
                Ok(file) => (file, false),
                Err(GuardianOutputError::Io { source, .. })
                    if source.kind() == ErrorKind::NotFound =>
                {
                    (
                        create_private_file_new_at(&directory, &self.directory_path, &path)?,
                        true,
                    )
                }
                Err(error) => return Err(error),
            };
        let opened = file
            .metadata()
            .map_err(|error| GuardianOutputError::io("input-journal-metadata", error))?;
        validate_private_file_metadata(&opened, None)?;
        let file_identity = FileIdentity::capture(&opened, None);
        validate_file_identity_at(&directory, &self.directory_path, &path, file_identity)?;
        if !created {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian input WAL reopen requires durable anti-rollback authority",
            ));
        }
        let mut journal = GuardianInputJournal::create(
            file,
            pane_id,
            guardian_incarnation,
            self.cipher.clone(),
            GuardianInputJournalLimits::default(),
        )?;
        journal.sync_parent_directory_and_activate(&directory)?;
        // Synchronize the directory again after the journal header itself is
        // durable, before treating this newly created path as spawn authority.
        directory
            .sync_all()
            .map_err(|error| GuardianOutputError::io("input-directory-sync", error))?;
        if journal.record_count() != 0
            || journal.effects().len() != 0
            || journal.tail() != mux::guardian_input_journal::GuardianInputJournalTail::Clean
            || journal.is_poisoned()
        {
            return Err(GuardianOutputError::FilesystemAuthority(
                "new guardian input WAL initialized with unexpected state",
            ));
        }
        let input = GuardianPaneInputJournal {
            journal,
            directory,
            directory_path: self.directory_path.clone(),
            path,
            file_identity,
            persistence: Arc::clone(&self.persistence),
        };
        input.validate_path_authority()?;
        Ok(input)
    }

    #[cfg(test)]
    fn cold_open_pane_for_validation(
        &self,
        guardian_incarnation: Uuid,
        pane_id: Uuid,
    ) -> Result<GuardianPaneOutputJournal, GuardianOutputError> {
        let authority = open_existing_pane_segment_manager(
            &self.directory,
            &self.directory_path,
            Arc::clone(&self.persistence),
            self.cipher.clone(),
            self.policy,
            guardian_incarnation,
            pane_id,
        )?;
        pane_journal_handle(authority)
    }

    #[cfg(test)]
    fn relevant_pane_paths(
        &self,
        guardian_incarnation: Uuid,
        pane_id: Uuid,
    ) -> Result<Vec<PathBuf>, GuardianOutputError> {
        list_relevant_pane_paths(&self.directory_path, guardian_incarnation, pane_id)
    }

    #[cfg(test)]
    fn publish_torn_manifest_candidate(
        &self,
        guardian_incarnation: Uuid,
        pane_id: Uuid,
        revision: u64,
    ) -> Result<PathBuf, GuardianOutputError> {
        let manifest_id = Uuid::new_v4();
        let path = manifest_path(
            &self.directory_path,
            guardian_incarnation,
            pane_id,
            revision,
            manifest_id,
        );
        let mut file = create_private_file_new_at(&self.directory, &self.directory_path, &path)?;
        file.write_all(b"torn-manifest-crash-cut")
            .map_err(|error| GuardianOutputError::io("output-manifest-crash-cut-write", error))?;
        file.sync_all()
            .map_err(|error| GuardianOutputError::io("output-manifest-crash-cut-sync", error))?;
        self.directory.sync_all().map_err(|error| {
            GuardianOutputError::io("output-manifest-crash-cut-dir-sync", error)
        })?;
        Ok(path)
    }
}

fn pane_journal_handle(
    authority: PaneJournalAuthority,
) -> Result<GuardianPaneOutputJournal, GuardianOutputError> {
    let initial_next_sequence = authority.current_journal.next_sequence();
    let initial_cumulative_plaintext_bytes = authority.current_journal.cumulative_plaintext_bytes();
    let initial_remaining_records = authority.remaining_records()?;
    if initial_next_sequence.is_none()
        || initial_remaining_records == 0
        || !authority.can_accept_min_record()
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output chain has no append capacity",
        ));
    }
    Ok(GuardianPaneOutputJournal {
        authority: Arc::new(Mutex::new(authority)),
        initial_next_sequence,
        initial_cumulative_plaintext_bytes,
        initial_remaining_records,
    })
}

impl GuardianOutputPipeline {
    pub(crate) fn available_slots(&self) -> usize {
        self.queue.available_slots()
    }

    pub(crate) fn try_submit(
        &self,
        pane_id: Uuid,
        journal: GuardianPaneOutputJournal,
        payload: Zeroizing<Vec<u8>>,
    ) -> Result<(), GuardianOutputSubmitError> {
        if payload.is_empty() || payload.len() > OUTPUT_RECORD_BYTES {
            return Err(GuardianOutputSubmitError::Unavailable(payload));
        }
        let job = OutputJob {
            pane_id,
            journal,
            payload,
        };
        self.queue.try_push(job).map_err(|error| match error {
            OutputQueuePushError::Saturated(job) => {
                GuardianOutputSubmitError::Saturated(job.payload)
            }
            OutputQueuePushError::Shutdown(job) => {
                GuardianOutputSubmitError::Unavailable(job.payload)
            }
        })
    }

    pub(crate) fn try_completion(&self) -> GuardianOutputCompletionState {
        let Some(completions) = self.completions.as_ref() else {
            return GuardianOutputCompletionState::Disconnected;
        };
        match completions.try_recv() {
            Ok(completion) => GuardianOutputCompletionState::Ready(completion),
            Err(TryRecvError::Empty) => GuardianOutputCompletionState::Empty,
            Err(TryRecvError::Disconnected) => GuardianOutputCompletionState::Disconnected,
        }
    }
}

impl Drop for GuardianOutputPipeline {
    fn drop(&mut self) {
        drop(self.completions.take());
        self.queue.shutdown();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn output_worker(
    queue: &OutputQueue,
    completions: &SyncSender<GuardianOutputCompletion>,
    completion_waker: &Waker,
) {
    while let Some(mut job) = queue.pop() {
        let payload_bytes = job.payload.len();
        let mut result = match job.journal.authority.lock() {
            Ok(mut authority) => authority.append_and_sync(job.payload.as_slice()),
            Err(_) => Err(OutputCommitError::JournalLockPoisoned),
        };
        job.payload.zeroize();
        drop(job.payload);
        if !queue.complete_one() {
            result = Err(OutputCommitError::QueueInvariant);
        }
        let completion = GuardianOutputCompletion {
            pane_id: job.pane_id,
            payload_bytes,
            result: result.map_err(|_| GuardianOutputCommitFailure),
        };
        if completions.send(completion).is_err() {
            return;
        }
        let _ = completion_waker.wake();
    }
}

fn output_frame_bytes(payload_bytes: usize) -> Result<u64, OutputCommitError> {
    u64::try_from(payload_bytes)
        .ok()
        .and_then(|bytes| bytes.checked_add(OUTPUT_V3_RECORD_OVERHEAD_BYTES))
        .ok_or(OutputCommitError::Capacity)
}

fn minimum_segment_bytes(payload_bytes: usize) -> Result<u64, GuardianOutputError> {
    u64::try_from(payload_bytes)
        .ok()
        .and_then(|bytes| bytes.checked_add(OUTPUT_V3_RECORD_OVERHEAD_BYTES))
        .and_then(|bytes| bytes.checked_add(OUTPUT_V3_FILE_HEADER_BYTES))
        .ok_or(GuardianOutputError::Allocation)
}

fn manifest_encoded_bytes(segment_count: usize) -> Result<u64, GuardianOutputError> {
    if segment_count == 0 || segment_count > OUTPUT_MAX_SEGMENTS_PER_PANE {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest segment count is outside its hard bound",
        ));
    }
    OUTPUT_MANIFEST_HEADER_BYTES
        .checked_add(
            segment_count
                .checked_mul(OUTPUT_MANIFEST_SEGMENT_BYTES)
                .ok_or(GuardianOutputError::Allocation)?,
        )
        .and_then(|bytes| bytes.checked_add(OUTPUT_MANIFEST_CHECKSUM_BYTES))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(GuardianOutputError::Allocation)
}

fn journal_can_append(
    journal: &GuardianOutputJournal,
    limits: GuardianOutputJournalLimits,
    payload_bytes: usize,
) -> Result<bool, OutputCommitError> {
    let projected = journal
        .committed_bytes()
        .checked_add(output_frame_bytes(payload_bytes)?)
        .ok_or(OutputCommitError::Capacity)?;
    Ok(!journal.is_poisoned()
        && journal.tail() == GuardianOutputJournalTail::Clean
        && !journal.directory_entry_sync_required()
        && journal.next_sequence().is_some()
        && journal.record_count() < limits.max_records
        && projected <= limits.max_log_bytes)
}

fn pane_file_prefix(guardian_incarnation: Uuid, pane_id: Uuid) -> String {
    format!("pane-{pane_id}.guardian-{guardian_incarnation}.")
}

fn input_journal_path(directory_path: &Path, guardian_incarnation: Uuid, pane_id: Uuid) -> PathBuf {
    // This intentionally does not begin with `pane_file_prefix`: the output
    // manifest scanner must never count an input WAL as an output artifact.
    directory_path.join(format!(
        "input-pane-{pane_id}.guardian-{guardian_incarnation}.{INPUT_JOURNAL_SUFFIX}"
    ))
}

fn segment_path(
    directory_path: &Path,
    guardian_incarnation: Uuid,
    pane_id: Uuid,
    segment_id: Uuid,
) -> PathBuf {
    directory_path.join(format!(
        "{}segment-{segment_id}.ftgout",
        pane_file_prefix(guardian_incarnation, pane_id)
    ))
}

fn manifest_path(
    directory_path: &Path,
    guardian_incarnation: Uuid,
    pane_id: Uuid,
    revision: u64,
    manifest_id: Uuid,
) -> PathBuf {
    directory_path.join(format!(
        "{}manifest-{revision:020}-{manifest_id}.ftgmanifest",
        pane_file_prefix(guardian_incarnation, pane_id)
    ))
}

fn manifest_publication_path(
    directory_path: &Path,
    guardian_incarnation: Uuid,
    pane_id: Uuid,
    revision: u64,
    manifest_id: Uuid,
    checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
) -> PathBuf {
    directory_path.join(format!(
        "{}publication-{revision:020}-{manifest_id}-{}.ftgmanifestcommit",
        pane_file_prefix(guardian_incarnation, pane_id),
        checksum_hex(checksum)
    ))
}

fn checksum_hex(checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(OUTPUT_MANIFEST_CHECKSUM_BYTES * 2);
    for byte in checksum {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn parse_checksum_hex(encoded: &str) -> Option<[u8; OUTPUT_MANIFEST_CHECKSUM_BYTES]> {
    if encoded.len() != OUTPUT_MANIFEST_CHECKSUM_BYTES * 2 {
        return None;
    }
    let mut checksum = [0; OUTPUT_MANIFEST_CHECKSUM_BYTES];
    for (index, pair) in encoded.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        checksum[index] = (high << 4) | low;
    }
    Some(checksum)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn create_collision_resistant_segment(
    directory: &File,
    directory_path: &Path,
    pane_id: Uuid,
    guardian_incarnation: Uuid,
    first_sequence: u64,
    predecessor: Option<GuardianOutputPredecessor>,
    cipher: GuardianOutputCipher,
    limits: GuardianOutputJournalLimits,
) -> Result<(GuardianOutputJournal, SegmentPathAuthority), GuardianOutputError> {
    for _ in 0..OUTPUT_PATH_COLLISION_ATTEMPTS {
        let segment_id = Uuid::new_v4();
        let identity =
            GuardianOutputSegmentIdentity::new(pane_id, segment_id, first_sequence, predecessor)?;
        match create_segment_at_identity(
            directory,
            directory_path,
            guardian_incarnation,
            identity,
            cipher.clone(),
            limits,
        ) {
            Ok(created) => return Ok(created),
            Err(GuardianOutputError::Io { source, .. })
                if source.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(GuardianOutputError::FilesystemAuthority(
        "guardian output segment UUID collision bound exhausted",
    ))
}

fn create_segment_at_identity(
    directory: &File,
    directory_path: &Path,
    guardian_incarnation: Uuid,
    identity: GuardianOutputSegmentIdentity,
    cipher: GuardianOutputCipher,
    limits: GuardianOutputJournalLimits,
) -> Result<(GuardianOutputJournal, SegmentPathAuthority), GuardianOutputError> {
    let path = segment_path(
        directory_path,
        guardian_incarnation,
        identity.durable_pane_id(),
        identity.segment_id(),
    );
    let file = create_private_file_new_at(directory, directory_path, &path)?;
    let file_identity = FileIdentity::capture(
        &file
            .metadata()
            .map_err(|error| GuardianOutputError::io("output-segment-created-metadata", error))?,
        None,
    );
    let mut journal = GuardianOutputJournal::open(file, identity, cipher, limits)?;
    journal.sync_parent_directory_and_activate(directory)?;
    validate_file_identity_at(directory, directory_path, &path, file_identity)?;
    if journal.tail() != GuardianOutputJournalTail::Clean
        || journal.is_poisoned()
        || journal.directory_entry_sync_required()
        || journal.committed_bytes() != OUTPUT_V3_FILE_HEADER_BYTES
        || journal.record_count() != 0
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "new guardian output segment is not an empty durable header",
        ));
    }
    Ok((
        journal,
        SegmentPathAuthority {
            segment_identity: identity,
            path,
            file_identity,
        },
    ))
}

fn publish_initial_manifest(
    directory: &File,
    directory_path: &Path,
    pane_id: Uuid,
    guardian_incarnation: Uuid,
    segments: Vec<GuardianOutputSegmentIdentity>,
) -> Result<PublishedManifestAuthority, GuardianOutputError> {
    publish_manifest_snapshot(
        directory,
        directory_path,
        pane_id,
        guardian_incarnation,
        1,
        None,
        segments,
    )
}

fn publish_successor_manifest(
    directory: &File,
    directory_path: &Path,
    previous: &OutputManifestSnapshot,
    segments: Vec<GuardianOutputSegmentIdentity>,
) -> Result<PublishedManifestAuthority, GuardianOutputError> {
    let revision = previous
        .revision
        .checked_add(1)
        .ok_or(GuardianOutputError::Allocation)?;
    publish_manifest_snapshot(
        directory,
        directory_path,
        previous.durable_pane_id,
        previous.guardian_incarnation,
        revision,
        Some(ManifestPredecessor {
            manifest_id: previous.manifest_id,
            checksum: previous.checksum,
        }),
        segments,
    )
}

fn publish_manifest_snapshot(
    directory: &File,
    directory_path: &Path,
    pane_id: Uuid,
    guardian_incarnation: Uuid,
    revision: u64,
    predecessor: Option<ManifestPredecessor>,
    segments: Vec<GuardianOutputSegmentIdentity>,
) -> Result<PublishedManifestAuthority, GuardianOutputError> {
    validate_manifest_structure(revision, predecessor, &segments)?;
    for _ in 0..OUTPUT_PATH_COLLISION_ATTEMPTS {
        let manifest_id = Uuid::new_v4();
        let mut snapshot = OutputManifestSnapshot {
            durable_pane_id: pane_id,
            guardian_incarnation,
            manifest_id,
            revision,
            predecessor,
            segments: segments.clone(),
            checksum: [0; OUTPUT_MANIFEST_CHECKSUM_BYTES],
        };
        let encoded = encode_manifest(&mut snapshot)?;
        let path = manifest_path(
            directory_path,
            guardian_incarnation,
            pane_id,
            revision,
            manifest_id,
        );
        let mut file = match create_private_file_new_at(directory, directory_path, &path) {
            Ok(file) => file,
            Err(GuardianOutputError::Io { source, .. })
                if source.kind() == ErrorKind::AlreadyExists =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        file.write_all(&encoded)
            .map_err(|error| GuardianOutputError::io("output-manifest-write", error))?;
        file.sync_all()
            .map_err(|error| GuardianOutputError::io("output-manifest-sync", error))?;
        let metadata = file
            .metadata()
            .map_err(|error| GuardianOutputError::io("output-manifest-metadata", error))?;
        validate_private_file_metadata(
            &metadata,
            Some(u64::try_from(encoded.len()).map_err(|_| GuardianOutputError::Allocation)?),
        )?;
        let identity = FileIdentity::capture(&metadata, Some(metadata.len()));
        validate_file_identity_at(directory, directory_path, &path, identity)?;
        // Make the immutable candidate name durable before publishing its
        // zero-length commit marker. If the marker survives a crash, the exact
        // checksum-bound candidate it names was already directory-synchronized.
        directory.sync_all().map_err(|error| {
            GuardianOutputError::io("output-manifest-candidate-directory-sync", error)
        })?;
        validate_file_identity_at(directory, directory_path, &path, identity)?;
        let publication_path = manifest_publication_path(
            directory_path,
            guardian_incarnation,
            pane_id,
            revision,
            manifest_id,
            snapshot.checksum,
        );
        let publication = create_private_file_new_at(directory, directory_path, &publication_path)?;
        publication
            .sync_all()
            .map_err(|error| GuardianOutputError::io("output-manifest-publication-sync", error))?;
        let publication_metadata = publication.metadata().map_err(|error| {
            GuardianOutputError::io("output-manifest-publication-metadata", error)
        })?;
        validate_private_file_metadata(&publication_metadata, Some(0))?;
        let publication_identity = FileIdentity::capture(&publication_metadata, Some(0));
        validate_file_identity_at(
            directory,
            directory_path,
            &publication_path,
            publication_identity,
        )?;
        directory
            .sync_all()
            .map_err(|error| GuardianOutputError::io("output-manifest-directory-sync", error))?;
        validate_file_identity_at(directory, directory_path, &path, identity)?;
        validate_file_identity_at(
            directory,
            directory_path,
            &publication_path,
            publication_identity,
        )?;
        return Ok(PublishedManifestAuthority {
            snapshot,
            path,
            file_identity: identity,
            publication_path,
            publication_file_identity: publication_identity,
        });
    }
    Err(GuardianOutputError::FilesystemAuthority(
        "guardian output manifest UUID collision bound exhausted",
    ))
}

fn validate_manifest_structure(
    revision: u64,
    predecessor: Option<ManifestPredecessor>,
    segments: &[GuardianOutputSegmentIdentity],
) -> Result<(), GuardianOutputError> {
    if revision == 0
        || usize::try_from(revision).ok() != Some(segments.len())
        || segments.is_empty()
        || segments.len() > OUTPUT_MAX_SEGMENTS_PER_PANE
        || (revision == 1) != predecessor.is_none()
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest structure is invalid",
        ));
    }
    for (index, identity) in segments.iter().copied().enumerate() {
        if index == 0 {
            if identity.first_sequence() != 1 || identity.predecessor().is_some() {
                return Err(GuardianOutputError::FilesystemAuthority(
                    "guardian output manifest initial segment is invalid",
                ));
            }
        } else {
            let Some(segment_predecessor) = identity.predecessor() else {
                return Err(GuardianOutputError::FilesystemAuthority(
                    "guardian output manifest successor has no predecessor",
                ));
            };
            if segment_predecessor.segment_id() != segments[index - 1].segment_id() {
                return Err(GuardianOutputError::FilesystemAuthority(
                    "guardian output manifest segment order is not contiguous",
                ));
            }
        }
    }
    Ok(())
}

fn encode_manifest(snapshot: &mut OutputManifestSnapshot) -> Result<Vec<u8>, GuardianOutputError> {
    validate_manifest_structure(snapshot.revision, snapshot.predecessor, &snapshot.segments)?;
    let body_bytes = OUTPUT_MANIFEST_HEADER_BYTES
        .checked_add(
            snapshot
                .segments
                .len()
                .checked_mul(OUTPUT_MANIFEST_SEGMENT_BYTES)
                .ok_or(GuardianOutputError::Allocation)?,
        )
        .ok_or(GuardianOutputError::Allocation)?;
    let total_bytes = body_bytes
        .checked_add(OUTPUT_MANIFEST_CHECKSUM_BYTES)
        .ok_or(GuardianOutputError::Allocation)?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(total_bytes)
        .map_err(|_| GuardianOutputError::Allocation)?;
    encoded.extend_from_slice(&OUTPUT_MANIFEST_MAGIC);
    encoded.extend_from_slice(&OUTPUT_MANIFEST_VERSION.to_le_bytes());
    encoded.extend_from_slice(snapshot.durable_pane_id.as_bytes());
    encoded.extend_from_slice(snapshot.guardian_incarnation.as_bytes());
    encoded.extend_from_slice(snapshot.manifest_id.as_bytes());
    encoded.extend_from_slice(&snapshot.revision.to_le_bytes());
    match snapshot.predecessor {
        Some(previous) => {
            encoded.push(1);
            encoded.extend_from_slice(&[0; 7]);
            encoded.extend_from_slice(previous.manifest_id.as_bytes());
            encoded.extend_from_slice(&previous.checksum);
        }
        None => {
            encoded.extend_from_slice(&[0; 8]);
            encoded.extend_from_slice(&[0; 16]);
            encoded.extend_from_slice(&[0; OUTPUT_MANIFEST_CHECKSUM_BYTES]);
        }
    }
    encoded.extend_from_slice(
        &u32::try_from(snapshot.segments.len())
            .map_err(|_| GuardianOutputError::Allocation)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(&[0; 4]);
    for identity in snapshot.segments.iter().copied() {
        encoded.extend_from_slice(identity.segment_id().as_bytes());
        encoded.extend_from_slice(&identity.first_sequence().to_le_bytes());
        match identity.predecessor() {
            Some(previous) => {
                encoded.push(1);
                encoded.extend_from_slice(&[0; 7]);
                encoded.extend_from_slice(previous.segment_id().as_bytes());
                encoded.extend_from_slice(&previous.last_sequence().to_le_bytes());
                encoded.extend_from_slice(&previous.terminal_record_digest());
                encoded.extend_from_slice(&previous.cumulative_plaintext_bytes().to_le_bytes());
                encoded.extend_from_slice(&previous.committed_log_bytes().to_le_bytes());
            }
            None => encoded.extend_from_slice(&[0; OUTPUT_MANIFEST_SEGMENT_BYTES - 24]),
        }
    }
    if encoded.len() != body_bytes {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest encoder length is inconsistent",
        ));
    }
    snapshot.checksum = manifest_checksum(&encoded);
    encoded.extend_from_slice(&snapshot.checksum);
    Ok(encoded)
}

fn manifest_checksum(bytes: &[u8]) -> [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(OUTPUT_MANIFEST_CHECKSUM_DOMAIN);
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut checksum = [0; OUTPUT_MANIFEST_CHECKSUM_BYTES];
    checksum.copy_from_slice(&digest);
    checksum
}

struct ManifestDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ManifestDecoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], GuardianOutputError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or(GuardianOutputError::Allocation)?;
        let source =
            self.bytes
                .get(self.offset..end)
                .ok_or(GuardianOutputError::FilesystemAuthority(
                    "guardian output manifest is truncated",
                ))?;
        let mut value = [0; N];
        value.copy_from_slice(source);
        self.offset = end;
        Ok(value)
    }

    fn u32(&mut self) -> Result<u32, GuardianOutputError> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> Result<u64, GuardianOutputError> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn uuid(&mut self) -> Result<Uuid, GuardianOutputError> {
        Ok(Uuid::from_bytes(self.take()?))
    }
}

fn decode_manifest(bytes: &[u8]) -> Result<OutputManifestSnapshot, GuardianOutputError> {
    let maximum = OUTPUT_MANIFEST_HEADER_BYTES
        .checked_add(
            OUTPUT_MAX_SEGMENTS_PER_PANE
                .checked_mul(OUTPUT_MANIFEST_SEGMENT_BYTES)
                .ok_or(GuardianOutputError::Allocation)?,
        )
        .and_then(|body| body.checked_add(OUTPUT_MANIFEST_CHECKSUM_BYTES))
        .ok_or(GuardianOutputError::Allocation)?;
    if bytes.len() < OUTPUT_MANIFEST_HEADER_BYTES + OUTPUT_MANIFEST_CHECKSUM_BYTES
        || bytes.len() > maximum
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest byte length is invalid",
        ));
    }
    let checksum_offset = bytes
        .len()
        .checked_sub(OUTPUT_MANIFEST_CHECKSUM_BYTES)
        .ok_or(GuardianOutputError::Allocation)?;
    let mut observed_checksum = [0; OUTPUT_MANIFEST_CHECKSUM_BYTES];
    observed_checksum.copy_from_slice(&bytes[checksum_offset..]);
    let expected_checksum = manifest_checksum(&bytes[..checksum_offset]);
    if observed_checksum
        .iter()
        .zip(expected_checksum.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        != 0
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest checksum mismatch",
        ));
    }
    let mut decoder = ManifestDecoder::new(&bytes[..checksum_offset]);
    if decoder.take::<8>()? != OUTPUT_MANIFEST_MAGIC || decoder.u32()? != OUTPUT_MANIFEST_VERSION {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest magic or version is invalid",
        ));
    }
    let durable_pane_id = decoder.uuid()?;
    let guardian_incarnation = decoder.uuid()?;
    let manifest_id = decoder.uuid()?;
    let revision = decoder.u64()?;
    let predecessor_present = decoder.take::<1>()?[0];
    if decoder.take::<7>()? != [0; 7] {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest reserved header bytes are nonzero",
        ));
    }
    let predecessor_id = decoder.uuid()?;
    let predecessor_checksum = decoder.take::<OUTPUT_MANIFEST_CHECKSUM_BYTES>()?;
    let predecessor = match predecessor_present {
        0 if predecessor_id.is_nil()
            && predecessor_checksum == [0; OUTPUT_MANIFEST_CHECKSUM_BYTES] =>
        {
            None
        }
        1 if !predecessor_id.is_nil() => Some(ManifestPredecessor {
            manifest_id: predecessor_id,
            checksum: predecessor_checksum,
        }),
        _ => {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output manifest predecessor encoding is invalid",
            ));
        }
    };
    let segment_count =
        usize::try_from(decoder.u32()?).map_err(|_| GuardianOutputError::Allocation)?;
    if decoder.u32()? != 0 {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest reserved count bytes are nonzero",
        ));
    }
    let expected_body = OUTPUT_MANIFEST_HEADER_BYTES
        .checked_add(
            segment_count
                .checked_mul(OUTPUT_MANIFEST_SEGMENT_BYTES)
                .ok_or(GuardianOutputError::Allocation)?,
        )
        .ok_or(GuardianOutputError::Allocation)?;
    if expected_body != checksum_offset {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest segment count does not match its length",
        ));
    }
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(segment_count)
        .map_err(|_| GuardianOutputError::Allocation)?;
    for _ in 0..segment_count {
        let segment_id = decoder.uuid()?;
        let first_sequence = decoder.u64()?;
        let predecessor_present = decoder.take::<1>()?[0];
        if decoder.take::<7>()? != [0; 7] {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output manifest segment reserved bytes are nonzero",
            ));
        }
        let previous_segment_id = decoder.uuid()?;
        let previous_last_sequence = decoder.u64()?;
        let previous_digest = decoder.take::<32>()?;
        let previous_cumulative = decoder.u64()?;
        let previous_committed = decoder.u64()?;
        let segment_predecessor = match predecessor_present {
            0 if previous_segment_id.is_nil()
                && previous_last_sequence == 0
                && previous_digest == [0; 32]
                && previous_cumulative == 0
                && previous_committed == 0 =>
            {
                None
            }
            1 => Some(GuardianOutputPredecessor::new(
                previous_segment_id,
                previous_last_sequence,
                previous_digest,
                previous_cumulative,
                previous_committed,
            )?),
            _ => {
                return Err(GuardianOutputError::FilesystemAuthority(
                    "guardian output manifest segment predecessor is invalid",
                ));
            }
        };
        segments.push(GuardianOutputSegmentIdentity::new(
            durable_pane_id,
            segment_id,
            first_sequence,
            segment_predecessor,
        )?);
    }
    if decoder.offset != checksum_offset
        || durable_pane_id.is_nil()
        || guardian_incarnation.is_nil()
        || manifest_id.is_nil()
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest identity is invalid",
        ));
    }
    validate_manifest_structure(revision, predecessor, &segments)?;
    Ok(OutputManifestSnapshot {
        durable_pane_id,
        guardian_incarnation,
        manifest_id,
        revision,
        predecessor,
        segments,
        checksum: observed_checksum,
    })
}

#[derive(Clone)]
struct DiscoveredManifest {
    snapshot: OutputManifestSnapshot,
    path: PathBuf,
    file_identity: FileIdentity,
    publication_path: PathBuf,
    publication_file_identity: FileIdentity,
}

struct DiscoveredManifestCandidate {
    revision: u64,
    manifest_id: Uuid,
    snapshot: Option<OutputManifestSnapshot>,
    path: PathBuf,
    file_identity: FileIdentity,
    published: bool,
}

struct DiscoveredManifestPublication {
    revision: u64,
    manifest_id: Uuid,
    checksum: [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES],
    path: PathBuf,
    file_identity: FileIdentity,
}

#[derive(Clone)]
struct DiscoveredSegment {
    segment_id: Uuid,
    path: PathBuf,
    file_identity: FileIdentity,
    bytes: u64,
}

struct PanePublicationScan {
    head: Option<DiscoveredManifest>,
    manifest_history: Vec<ManifestPathAuthority>,
    segments: Vec<DiscoveredSegment>,
    relevant_files: usize,
    relevant_file_bytes: u64,
}

fn scan_pane_publications(
    directory: &File,
    directory_path: &Path,
    guardian_incarnation: Uuid,
    pane_id: Uuid,
) -> Result<PanePublicationScan, GuardianOutputError> {
    let prefix = pane_file_prefix(guardian_incarnation, pane_id);
    let mut manifest_candidates = Vec::new();
    let mut manifest_publications = Vec::new();
    let mut segments = Vec::new();
    manifest_candidates
        .try_reserve_exact(OUTPUT_MAX_RELEVANT_FILES_PER_PANE)
        .map_err(|_| GuardianOutputError::Allocation)?;
    manifest_publications
        .try_reserve_exact(OUTPUT_MAX_RELEVANT_FILES_PER_PANE)
        .map_err(|_| GuardianOutputError::Allocation)?;
    segments
        .try_reserve_exact(OUTPUT_MAX_SEGMENTS_PER_PANE)
        .map_err(|_| GuardianOutputError::Allocation)?;
    let mut relevant_files = 0_usize;
    let mut relevant_file_bytes = 0_u64;
    for file_name in read_directory_names(directory)? {
        let Some(name) = file_name.to_str().map(str::to_owned) else {
            continue;
        };
        let Some(remainder) = name.strip_prefix(&prefix) else {
            continue;
        };
        relevant_files = relevant_files
            .checked_add(1)
            .ok_or(GuardianOutputError::Allocation)?;
        if relevant_files > OUTPUT_MAX_RELEVANT_FILES_PER_PANE {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output pane publication file bound is exhausted",
            ));
        }
        let path = directory_path.join(&file_name);
        let opened = open_private_file_at(directory, directory_path, &path, false)?;
        let opened_metadata = opened.metadata().map_err(|error| {
            GuardianOutputError::io("output-publication-opened-metadata", error)
        })?;
        validate_private_file_metadata(&opened_metadata, None)?;
        validate_file_identity_at(
            directory,
            directory_path,
            &path,
            FileIdentity::capture(&opened_metadata, None),
        )?;
        relevant_file_bytes = relevant_file_bytes
            .checked_add(opened_metadata.len())
            .ok_or(GuardianOutputError::Allocation)?;

        if let Some(segment_id) = parse_segment_file_name(remainder) {
            if segments.len() >= OUTPUT_MAX_SEGMENTS_PER_PANE {
                return Err(GuardianOutputError::FilesystemAuthority(
                    "guardian output physical segment count exceeds its hard bound",
                ));
            }
            if segments
                .iter()
                .any(|segment: &DiscoveredSegment| segment.segment_id == segment_id)
            {
                return Err(GuardianOutputError::FilesystemAuthority(
                    "guardian output segment UUID is published more than once",
                ));
            }
            segments.push(DiscoveredSegment {
                segment_id,
                path,
                file_identity: FileIdentity::capture(&opened_metadata, None),
                bytes: opened_metadata.len(),
            });
            continue;
        }
        if let Some((file_revision, file_manifest_id)) = parse_manifest_file_name(remainder) {
            let manifest_identity =
                FileIdentity::capture(&opened_metadata, Some(opened_metadata.len()));
            let bytes = read_manifest_file_bounded(opened, &opened_metadata)?;
            validate_file_identity_at(directory, directory_path, &path, manifest_identity)?;
            let snapshot = decode_manifest(&bytes).ok();
            if snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.durable_pane_id != pane_id
                    || snapshot.guardian_incarnation != guardian_incarnation
                    || snapshot.revision != file_revision
                    || snapshot.manifest_id != file_manifest_id
            }) {
                return Err(GuardianOutputError::FilesystemAuthority(
                    "guardian output manifest filename does not match its checksum-bound identity",
                ));
            }
            manifest_candidates.push(DiscoveredManifestCandidate {
                revision: file_revision,
                manifest_id: file_manifest_id,
                snapshot,
                path,
                file_identity: manifest_identity,
                published: false,
            });
            continue;
        }
        if let Some((revision, manifest_id, checksum)) =
            parse_manifest_publication_file_name(remainder)
        {
            validate_private_file_metadata(&opened_metadata, Some(0))?;
            let file_identity = FileIdentity::capture(&opened_metadata, Some(0));
            validate_file_identity_at(directory, directory_path, &path, file_identity)?;
            manifest_publications.push(DiscoveredManifestPublication {
                revision,
                manifest_id,
                checksum,
                path,
                file_identity,
            });
            continue;
        }
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output pane publication path is not canonical",
        ));
    }
    let manifests = pair_manifest_publications(&mut manifest_candidates, manifest_publications)?;
    let (head, manifest_history) = select_manifest_chain(manifests)?;
    Ok(PanePublicationScan {
        head,
        manifest_history,
        segments,
        relevant_files,
        relevant_file_bytes,
    })
}

fn pair_manifest_publications(
    candidates: &mut [DiscoveredManifestCandidate],
    publications: Vec<DiscoveredManifestPublication>,
) -> Result<Vec<DiscoveredManifest>, GuardianOutputError> {
    let mut manifests = Vec::new();
    manifests
        .try_reserve_exact(publications.len())
        .map_err(|_| GuardianOutputError::Allocation)?;
    for publication in publications {
        let candidate = candidates
            .iter_mut()
            .find(|candidate| {
                candidate.revision == publication.revision
                    && candidate.manifest_id == publication.manifest_id
            })
            .ok_or(GuardianOutputError::FilesystemAuthority(
                "guardian output publication marker has no immutable manifest candidate",
            ))?;
        if candidate.published {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output manifest has more than one publication marker",
            ));
        }
        let snapshot =
            candidate
                .snapshot
                .clone()
                .ok_or(GuardianOutputError::FilesystemAuthority(
                    "published guardian output manifest is torn or corrupt",
                ))?;
        if snapshot.checksum != publication.checksum {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output publication marker checksum does not match its manifest",
            ));
        }
        candidate.published = true;
        manifests.push(DiscoveredManifest {
            snapshot,
            path: candidate.path.clone(),
            file_identity: candidate.file_identity,
            publication_path: publication.path,
            publication_file_identity: publication.file_identity,
        });
    }
    Ok(manifests)
}

fn select_manifest_chain(
    mut manifests: Vec<DiscoveredManifest>,
) -> Result<(Option<DiscoveredManifest>, Vec<ManifestPathAuthority>), GuardianOutputError> {
    if manifests.is_empty() {
        return Ok((None, Vec::new()));
    }
    manifests.sort_by_key(|candidate| candidate.snapshot.revision);
    let mut manifest_history = Vec::new();
    manifest_history
        .try_reserve_exact(manifests.len())
        .map_err(|_| GuardianOutputError::Allocation)?;
    let mut iterator = manifests.into_iter();
    let mut head = iterator.next().ok_or(GuardianOutputError::Allocation)?;
    if head.snapshot.revision != 1 || head.snapshot.predecessor.is_some() {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest history has no unique initial publication",
        ));
    }
    manifest_history.push(ManifestPathAuthority {
        path: head.path.clone(),
        file_identity: head.file_identity,
        publication_path: head.publication_path.clone(),
        publication_file_identity: head.publication_file_identity,
    });
    for candidate in iterator {
        let expected_revision = head
            .snapshot
            .revision
            .checked_add(1)
            .ok_or(GuardianOutputError::Allocation)?;
        let expected_predecessor = ManifestPredecessor {
            manifest_id: head.snapshot.manifest_id,
            checksum: head.snapshot.checksum,
        };
        if candidate.snapshot.revision != expected_revision
            || candidate.snapshot.predecessor != Some(expected_predecessor)
            || candidate.snapshot.segments.len() != head.snapshot.segments.len().saturating_add(1)
            || candidate.snapshot.segments[..head.snapshot.segments.len()] != head.snapshot.segments
        {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output manifest history forks, gaps, or rewrites its prefix",
            ));
        }
        manifest_history.push(ManifestPathAuthority {
            path: candidate.path.clone(),
            file_identity: candidate.file_identity,
            publication_path: candidate.publication_path.clone(),
            publication_file_identity: candidate.publication_file_identity,
        });
        head = candidate;
    }
    Ok((Some(head), manifest_history))
}

fn parse_segment_file_name(remainder: &str) -> Option<Uuid> {
    let segment_id = remainder
        .strip_prefix("segment-")?
        .strip_suffix(".ftgout")?
        .parse()
        .ok()?;
    (remainder == format!("segment-{segment_id}.ftgout")).then_some(segment_id)
}

fn parse_manifest_file_name(remainder: &str) -> Option<(u64, Uuid)> {
    let body = remainder
        .strip_prefix("manifest-")?
        .strip_suffix(".ftgmanifest")?;
    let (revision, manifest_id) = body.split_once('-')?;
    if revision.len() != 20 || !revision.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let revision = revision.parse().ok()?;
    let manifest_id: Uuid = manifest_id.parse().ok()?;
    (remainder == format!("manifest-{revision:020}-{manifest_id}.ftgmanifest"))
        .then_some((revision, manifest_id))
}

fn parse_manifest_publication_file_name(
    remainder: &str,
) -> Option<(u64, Uuid, [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES])> {
    let body = remainder
        .strip_prefix("publication-")?
        .strip_suffix(".ftgmanifestcommit")?;
    let (revision, identity_and_checksum) = body.split_once('-')?;
    if revision.len() != 20 || !revision.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let manifest_id_text = identity_and_checksum.get(..36)?;
    if identity_and_checksum.as_bytes().get(36).copied()? != b'-' {
        return None;
    }
    let checksum_text = identity_and_checksum.get(37..)?;
    let revision = revision.parse().ok()?;
    let manifest_id: Uuid = manifest_id_text.parse().ok()?;
    let checksum = parse_checksum_hex(checksum_text)?;
    (remainder
        == format!(
            "publication-{revision:020}-{manifest_id}-{}.ftgmanifestcommit",
            checksum_hex(checksum)
        ))
    .then_some((revision, manifest_id, checksum))
}

fn read_manifest_file_bounded(
    mut file: File,
    metadata: &Metadata,
) -> Result<Vec<u8>, GuardianOutputError> {
    let maximum = OUTPUT_MANIFEST_HEADER_BYTES
        .checked_add(
            OUTPUT_MAX_SEGMENTS_PER_PANE
                .checked_mul(OUTPUT_MANIFEST_SEGMENT_BYTES)
                .ok_or(GuardianOutputError::Allocation)?,
        )
        .and_then(|bytes| bytes.checked_add(OUTPUT_MANIFEST_CHECKSUM_BYTES))
        .ok_or(GuardianOutputError::Allocation)?;
    let length = usize::try_from(metadata.len()).map_err(|_| GuardianOutputError::Allocation)?;
    if length > maximum {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest exceeds its byte bound",
        ));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| GuardianOutputError::Allocation)?;
    bytes.resize(length, 0);
    file.read_exact(&mut bytes)
        .map_err(|error| GuardianOutputError::io("output-manifest-read", error))?;
    let mut trailing = [0; 1];
    if file
        .read(&mut trailing)
        .map_err(|error| GuardianOutputError::io("output-manifest-trailing-read", error))?
        != 0
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest changed while it was read",
        ));
    }
    Ok(bytes)
}

fn open_or_create_pane_segment_manager(
    directory: &File,
    directory_path: &Path,
    persistence: Arc<PersistentOutputAuthority>,
    cipher: GuardianOutputCipher,
    policy: OutputSegmentPolicy,
    guardian_incarnation: Uuid,
    pane_id: Uuid,
) -> Result<PaneJournalAuthority, GuardianOutputError> {
    persistence.validate(directory).map_err(|_| {
        GuardianOutputError::FilesystemAuthority(
            "guardian output persistence authority changed before pane open",
        )
    })?;
    let scan = scan_pane_publications(directory, directory_path, guardian_incarnation, pane_id)?;
    if scan.head.is_some() {
        return open_scanned_pane_segment_manager(
            directory,
            directory_path,
            persistence,
            cipher,
            policy,
            guardian_incarnation,
            pane_id,
            scan,
        );
    }
    if scan.relevant_files != 0 {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output initial publication is incomplete; evidence was retained",
        ));
    }
    let (journal, segment) = create_collision_resistant_segment(
        directory,
        directory_path,
        pane_id,
        guardian_incarnation,
        1,
        None,
        cipher.clone(),
        policy.journal_limits,
    )?;
    let mut identities = Vec::new();
    identities
        .try_reserve_exact(1)
        .map_err(|_| GuardianOutputError::Allocation)?;
    identities.push(segment.segment_identity);
    let manifest = publish_initial_manifest(
        directory,
        directory_path,
        pane_id,
        guardian_incarnation,
        identities,
    )?;
    let mut manifest_history = Vec::new();
    manifest_history
        .try_reserve_exact(policy.max_segments)
        .map_err(|_| GuardianOutputError::Allocation)?;
    manifest_history.push(ManifestPathAuthority {
        path: manifest.path.clone(),
        file_identity: manifest.file_identity,
        publication_path: manifest.publication_path.clone(),
        publication_file_identity: manifest.publication_file_identity,
    });
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(policy.max_segments)
        .map_err(|_| GuardianOutputError::Allocation)?;
    segments.push(segment);
    let total_relevant_file_bytes = OUTPUT_V3_FILE_HEADER_BYTES
        .checked_add(manifest_encoded_bytes(1)?)
        .ok_or(GuardianOutputError::Allocation)?;
    let authority = PaneJournalAuthority {
        current_journal: journal,
        segments,
        manifest,
        manifest_history,
        directory: directory
            .try_clone()
            .map_err(|error| GuardianOutputError::io("output-directory-clone", error))?,
        directory_path: directory_path.to_path_buf(),
        cipher,
        policy,
        persistence,
        total_committed_log_bytes: OUTPUT_V3_FILE_HEADER_BYTES,
        total_relevant_file_bytes,
        total_records: 0,
        physical_segment_files: 1,
        relevant_files: 3,
        failed: false,
    };
    authority.validate_path_authority().map_err(|_| {
        GuardianOutputError::FilesystemAuthority(
            "guardian output initial publication authority changed",
        )
    })?;
    Ok(authority)
}

#[cfg(test)]
fn open_existing_pane_segment_manager(
    directory: &File,
    directory_path: &Path,
    persistence: Arc<PersistentOutputAuthority>,
    cipher: GuardianOutputCipher,
    policy: OutputSegmentPolicy,
    guardian_incarnation: Uuid,
    pane_id: Uuid,
) -> Result<PaneJournalAuthority, GuardianOutputError> {
    persistence.validate(directory).map_err(|_| {
        GuardianOutputError::FilesystemAuthority(
            "guardian output persistence authority changed before cold open",
        )
    })?;
    let scan = scan_pane_publications(directory, directory_path, guardian_incarnation, pane_id)?;
    if scan.head.is_none() {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output pane has no valid published manifest",
        ));
    }
    open_scanned_pane_segment_manager(
        directory,
        directory_path,
        persistence,
        cipher,
        policy,
        guardian_incarnation,
        pane_id,
        scan,
    )
}

fn open_scanned_pane_segment_manager(
    directory: &File,
    directory_path: &Path,
    persistence: Arc<PersistentOutputAuthority>,
    cipher: GuardianOutputCipher,
    policy: OutputSegmentPolicy,
    guardian_incarnation: Uuid,
    pane_id: Uuid,
    scan: PanePublicationScan,
) -> Result<PaneJournalAuthority, GuardianOutputError> {
    let discovered_head = scan.head.ok_or(GuardianOutputError::FilesystemAuthority(
        "guardian output pane has no manifest head",
    ))?;
    if discovered_head.snapshot.durable_pane_id != pane_id
        || discovered_head.snapshot.guardian_incarnation != guardian_incarnation
        || discovered_head.snapshot.segments.len() > policy.max_segments
        || scan.segments.len() > policy.max_segments
        || scan.relevant_file_bytes > policy.max_durable_pane_bytes
        || scan
            .segments
            .iter()
            .any(|segment| segment.bytes > policy.journal_limits.max_log_bytes)
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest authority is outside its pane policy",
        ));
    }
    let mut segment_paths = Vec::new();
    segment_paths
        .try_reserve_exact(policy.max_segments)
        .map_err(|_| GuardianOutputError::Allocation)?;
    for identity in discovered_head.snapshot.segments.iter().copied() {
        let discovered = scan
            .segments
            .iter()
            .find(|segment| segment.segment_id == identity.segment_id())
            .ok_or(GuardianOutputError::FilesystemAuthority(
                "guardian output manifest references a missing immutable segment",
            ))?;
        segment_paths.push(SegmentPathAuthority {
            segment_identity: identity,
            path: discovered.path.clone(),
            file_identity: discovered.file_identity,
        });
    }
    let (current_journal, total_committed_log_bytes, total_records) =
        open_and_validate_segment_chain(
            directory,
            directory_path,
            &segment_paths,
            &cipher,
            policy,
        )?;
    let authority = PaneJournalAuthority {
        current_journal,
        segments: segment_paths,
        manifest: PublishedManifestAuthority {
            snapshot: discovered_head.snapshot,
            path: discovered_head.path,
            file_identity: discovered_head.file_identity,
            publication_path: discovered_head.publication_path,
            publication_file_identity: discovered_head.publication_file_identity,
        },
        manifest_history: scan.manifest_history,
        directory: directory
            .try_clone()
            .map_err(|error| GuardianOutputError::io("output-directory-clone", error))?,
        directory_path: directory_path.to_path_buf(),
        cipher,
        policy,
        persistence,
        total_committed_log_bytes,
        total_relevant_file_bytes: scan.relevant_file_bytes,
        total_records,
        physical_segment_files: scan.segments.len(),
        relevant_files: scan.relevant_files,
        failed: false,
    };
    authority.validate_path_authority().map_err(|_| {
        GuardianOutputError::FilesystemAuthority(
            "guardian output cold-open publication authority changed",
        )
    })?;
    Ok(authority)
}

fn open_and_validate_segment_chain(
    directory: &File,
    directory_path: &Path,
    segments: &[SegmentPathAuthority],
    cipher: &GuardianOutputCipher,
    policy: OutputSegmentPolicy,
) -> Result<(GuardianOutputJournal, u64, u64), GuardianOutputError> {
    if segments.is_empty() || segments.len() > policy.max_segments {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output segment chain length is invalid",
        ));
    }
    let mut current = None;
    let mut previous_terminal = None;
    let mut total_committed_log_bytes = 0_u64;
    let mut total_records = 0_u64;
    for (index, segment) in segments.iter().enumerate() {
        validate_file_identity_at(
            directory,
            directory_path,
            &segment.path,
            segment.file_identity,
        )?;
        let file = open_private_file_at(directory, directory_path, &segment.path, false)?;
        let journal = GuardianOutputJournal::open(
            file,
            segment.segment_identity,
            cipher.clone(),
            policy.journal_limits,
        )?;
        if journal.directory_entry_sync_required()
            || journal.is_poisoned()
            || journal.tail() != GuardianOutputJournalTail::Clean
        {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output immutable segment is torn, poisoned, or unpublished",
            ));
        }
        if index == 0 {
            if segment.segment_identity.predecessor().is_some() {
                return Err(GuardianOutputError::FilesystemAuthority(
                    "guardian output initial segment has a predecessor",
                ));
            }
        } else if segment.segment_identity.predecessor() != previous_terminal {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output segment predecessor authority is not exact",
            ));
        }
        total_committed_log_bytes = total_committed_log_bytes
            .checked_add(journal.committed_bytes())
            .ok_or(GuardianOutputError::Allocation)?;
        total_records = total_records
            .checked_add(journal.record_count())
            .ok_or(GuardianOutputError::Allocation)?;
        if total_committed_log_bytes > policy.max_durable_pane_bytes {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output durable log byte policy is exhausted",
            ));
        }
        if index + 1 == segments.len() {
            current = Some(journal);
        } else {
            previous_terminal = journal
                .terminal_receipt()
                .map(GuardianOutputAppendReceipt::into_predecessor);
            if previous_terminal.is_none() {
                return Err(GuardianOutputError::FilesystemAuthority(
                    "guardian output nonterminal segment is empty",
                ));
            }
        }
    }
    Ok((
        current.ok_or(GuardianOutputError::FilesystemAuthority(
            "guardian output segment chain has no current segment",
        ))?,
        total_committed_log_bytes,
        total_records,
    ))
}

fn validate_replayable_segment_chain(
    directory: &File,
    directory_path: &Path,
    segments: &[SegmentPathAuthority],
    cipher: &GuardianOutputCipher,
    policy: OutputSegmentPolicy,
) -> Result<(), GuardianOutputError> {
    let _ = open_and_validate_segment_chain(directory, directory_path, segments, cipher, policy)?;
    Ok(())
}

#[cfg(test)]
fn recover_all_segment_bytes(
    authority: &PaneJournalAuthority,
) -> Result<Vec<u8>, GuardianOutputError> {
    validate_replayable_segment_chain(
        &authority.directory,
        &authority.directory_path,
        &authority.segments,
        &authority.cipher,
        authority.policy,
    )?;
    let mut recovered = Vec::new();
    for segment in &authority.segments {
        let file = open_private_file_at(
            &authority.directory,
            &authority.directory_path,
            &segment.path,
            false,
        )?;
        let journal = GuardianOutputJournal::open(
            file,
            segment.segment_identity,
            authority.cipher.clone(),
            authority.policy.journal_limits,
        )?;
        let mut cursor = journal.recovery_cursor(
            segment.segment_identity.first_sequence(),
            authority.policy.journal_limits.max_record_bytes,
        )?;
        while let Some(record) = cursor.next_record()? {
            record.into_authenticated_delivery()?.write_all_bounded(
                &mut recovered,
                authority.policy.journal_limits.max_record_bytes,
            )?;
        }
    }
    Ok(recovered)
}

#[cfg(test)]
fn list_relevant_pane_paths(
    directory_path: &Path,
    guardian_incarnation: Uuid,
    pane_id: Uuid,
) -> Result<Vec<PathBuf>, GuardianOutputError> {
    let prefix = pane_file_prefix(guardian_incarnation, pane_id);
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(directory_path)
        .map_err(|error| GuardianOutputError::io("output-test-directory-scan", error))?
    {
        let entry =
            entry.map_err(|error| GuardianOutputError::io("output-test-directory-entry", error))?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&prefix))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn output_child_name<'a>(
    directory_path: &Path,
    path: &'a Path,
) -> Result<&'a OsStr, GuardianOutputError> {
    if path.parent() != Some(directory_path) {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output child path escaped its pinned directory",
        ));
    }
    path.file_name().ok_or(GuardianOutputError::InvalidPath)
}

fn create_private_file_new_at(
    directory: &File,
    directory_path: &Path,
    path: &Path,
) -> Result<File, GuardianOutputError> {
    open_private_file_at(directory, directory_path, path, true)
}

fn open_output_directory(
    token_path: &Path,
) -> Result<(File, PathBuf, DirectoryIdentity, DirectoryIdentity), GuardianOutputError> {
    validate_normalized_absolute_file_path(token_path)?;
    let parent = token_path
        .parent()
        .ok_or(GuardianOutputError::InvalidPath)?;
    validate_private_directory(parent)?;
    let parent_before = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianOutputError::io("output-parent-metadata-before", error))?;
    validate_private_directory_metadata(&parent_before)?;
    let parent_directory = open_directory_no_follow(parent)?;
    let parent_opened = parent_directory
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-parent-opened-metadata", error))?;
    validate_private_directory_metadata(&parent_opened)?;
    require_same_directory_identity(
        &parent_before,
        &parent_opened,
        "guardian output parent identity changed while opening its descriptor",
    )?;
    let parent_after_open = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianOutputError::io("output-parent-metadata-after-open", error))?;
    require_same_directory_identity(
        &parent_opened,
        &parent_after_open,
        "guardian output parent identity changed after opening its descriptor",
    )?;

    let directory_path = parent.join(OUTPUT_DIRECTORY_NAME);
    match create_private_directory_at(&parent_directory, OsStr::new(OUTPUT_DIRECTORY_NAME)) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(GuardianOutputError::io("output-directory-create", error)),
    }
    let directory = open_private_directory_at(&parent_directory, OsStr::new(OUTPUT_DIRECTORY_NAME))
        .map_err(|error| GuardianOutputError::io("output-directory-open-at", error))?;
    let opened_directory = directory
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-directory-opened-metadata", error))?;
    validate_private_directory_metadata(&opened_directory)?;
    parent_directory
        .sync_all()
        .map_err(|error| GuardianOutputError::io("output-parent-directory-sync", error))?;
    let rebound_directory =
        open_private_directory_at(&parent_directory, OsStr::new(OUTPUT_DIRECTORY_NAME))
            .map_err(|error| GuardianOutputError::io("output-directory-reopen-at", error))?;
    let rebound_metadata = rebound_directory
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-directory-reopened-metadata", error))?;
    validate_private_directory_metadata(&rebound_metadata)?;
    require_same_directory_identity(
        &opened_directory,
        &rebound_metadata,
        "guardian output directory identity changed while rebinding its descriptor",
    )?;
    let parent_after = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianOutputError::io("output-parent-metadata-after", error))?;
    require_same_directory_identity(
        &parent_opened,
        &parent_after,
        "guardian output parent identity changed while opening its child directory",
    )?;
    let path_directory = std::fs::symlink_metadata(&directory_path)
        .map_err(|error| GuardianOutputError::io("output-directory-path-metadata", error))?;
    require_same_directory_identity(
        &opened_directory,
        &path_directory,
        "guardian output directory path no longer names its opened descriptor",
    )?;
    Ok((
        directory,
        directory_path,
        DirectoryIdentity::capture(&parent_after),
        DirectoryIdentity::capture(&opened_directory),
    ))
}

fn load_or_create_output_key(
    directory: &File,
    directory_path: &Path,
) -> Result<(GuardianOutputCipher, PathBuf, FileIdentity), GuardianOutputError> {
    validate_output_key_directory_authority(directory, directory_path)?;
    let key_path = directory_path.join(OUTPUT_KEY_NAME);
    let expected_len = u64::try_from(GuardianOutputCipher::KEY_BYTES)
        .map_err(|_| GuardianOutputError::Allocation)?;
    match open_private_file_at(directory, directory_path, &key_path, false) {
        Ok(file) => {
            let metadata = file
                .metadata()
                .map_err(|error| GuardianOutputError::io("output-key-metadata-at", error))?;
            validate_private_file_metadata(&metadata, Some(expected_len))?;
        }
        Err(GuardianOutputError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            ensure_absent_output_key_has_no_abandoned_ciphertext(
                directory,
                directory_path,
                &key_path,
                expected_len,
            )?;
        }
        Err(error) => return Err(error),
    }
    // Use the guardian's existing crash-restart-safe private-secret publisher:
    // it synchronizes a bounded stage plus digest-bound readiness record before
    // atomically moving the stage into this final name without replacement.
    // A crash therefore leaves either a resumable stage or a complete key,
    // never a partially written `journal.key` that blocks every later open.
    provision_guardian_token_in_pinned_parent(&key_path, directory).map_err(|_| {
        GuardianOutputError::FilesystemAuthority(
            "guardian output key provisioning did not reach a complete private authority",
        )
    })?;
    validate_output_key_directory_authority(directory, directory_path)?;

    let mut file = open_private_file_at(directory, directory_path, &key_path, false)?;
    let opened = file
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-key-opened-metadata", error))?;
    validate_private_file_metadata(&opened, Some(expected_len))?;
    let key = GuardianOutputKey::read_exact(&mut file)?;
    file.sync_all()
        .map_err(|error| GuardianOutputError::io("output-key-sync", error))?;
    directory
        .sync_all()
        .map_err(|error| GuardianOutputError::io("output-key-directory-sync", error))?;
    let opened_after = file
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-key-final-metadata", error))?;
    require_same_identity(
        &opened,
        &opened_after,
        Some(expected_len),
        "guardian output key identity changed while opening its contents",
    )?;
    validate_file_identity_at(
        directory,
        directory_path,
        &key_path,
        FileIdentity::capture(&opened_after, Some(expected_len)),
    )?;
    validate_output_key_directory_authority(directory, directory_path)?;
    let cipher = key.cipher()?;
    Ok((
        cipher,
        key_path,
        FileIdentity::capture(&opened_after, Some(expected_len)),
    ))
}

fn validate_output_key_directory_authority(
    directory: &File,
    directory_path: &Path,
) -> Result<(), GuardianOutputError> {
    let opened = directory
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-key-directory-metadata", error))?;
    validate_private_directory_metadata(&opened)?;
    let named = std::fs::symlink_metadata(directory_path)
        .map_err(|error| GuardianOutputError::io("output-key-directory-path-metadata", error))?;
    validate_private_directory_metadata(&named)?;
    require_same_directory_identity(
        &opened,
        &named,
        "guardian output key directory path no longer names its opened descriptor",
    )
}

fn ensure_absent_output_key_has_no_abandoned_ciphertext(
    directory: &File,
    directory_path: &Path,
    key_path: &Path,
    expected_key_bytes: u64,
) -> Result<(), GuardianOutputError> {
    let opened_before = directory
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-key-census-directory", error))?;
    validate_private_directory_metadata(&opened_before)?;
    let named_before = std::fs::symlink_metadata(directory_path)
        .map_err(|error| GuardianOutputError::io("output-key-census-path", error))?;
    require_same_directory_identity(
        &opened_before,
        &named_before,
        "guardian output key census began with a replaced directory path",
    )?;

    let stage_name = format!("{OUTPUT_KEY_NAME}.provisioning");
    let readiness_name = format!("{stage_name}.ready");
    let mut found_non_provisioning_entry = false;
    for name in read_directory_names(directory)? {
        if name != OsStr::new(&stage_name)
            && name != OsStr::new(&readiness_name)
            && name != OsStr::new(OUTPUT_KEY_NAME)
        {
            found_non_provisioning_entry = true;
        }
    }

    let opened_after = directory
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-key-census-directory-after", error))?;
    let named_after = std::fs::symlink_metadata(directory_path)
        .map_err(|error| GuardianOutputError::io("output-key-census-path-after", error))?;
    require_same_directory_identity(
        &opened_before,
        &opened_after,
        "guardian output key census changed the opened directory identity",
    )?;
    require_same_directory_identity(
        &opened_after,
        &named_after,
        "guardian output key census ended with a replaced directory path",
    )?;
    if !found_non_provisioning_entry {
        return Ok(());
    }

    // A concurrent provisioner may have installed the exact private key before
    // publishing its first pane artifact. Accept only that completed authority;
    // otherwise ciphertext/manifest evidence without its key must block creation
    // of a split replacement authority.
    match open_private_file_at(directory, directory_path, key_path, false) {
        Ok(file) => validate_private_file_metadata(
            &file.metadata().map_err(|error| {
                GuardianOutputError::io("output-key-census-final-metadata", error)
            })?,
            Some(expected_key_bytes),
        ),
        Err(GuardianOutputError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            Err(GuardianOutputError::FilesystemAuthority(
                "guardian output artifacts exist without their encryption key",
            ))
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn open_private_file_at(
    directory: &File,
    directory_path: &Path,
    path: &Path,
    create_new: bool,
) -> Result<File, GuardianOutputError> {
    let name = output_child_name(directory_path, path)?;
    let mut flags =
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW;
    if create_new {
        flags |= rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL;
    }
    rustix::fs::openat(
        directory,
        name,
        flags,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map(File::from)
    .map_err(|error| {
        GuardianOutputError::io("output-private-file-open-at", std::io::Error::from(error))
    })
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn open_private_file_at(
    _directory: &File,
    _directory_path: &Path,
    _path: &Path,
    _create_new: bool,
) -> Result<File, GuardianOutputError> {
    Err(GuardianOutputError::FilesystemAuthority(
        "descriptor-relative guardian output file access is unsupported on this Unix target",
    ))
}

fn validate_file_identity_at(
    directory: &File,
    directory_path: &Path,
    path: &Path,
    identity: FileIdentity,
) -> Result<(), GuardianOutputError> {
    let file = open_private_file_at(directory, directory_path, path, false)?;
    let metadata = file
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-file-identity-at", error))?;
    validate_private_file_metadata(&metadata, identity.expected_len)?;
    if !identity.matches(&metadata) {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output descriptor-relative file identity changed",
        ));
    }
    Ok(())
}

fn read_directory_names(directory: &File) -> Result<Vec<OsString>, GuardianOutputError> {
    let mut stream = rustix::fs::Dir::read_from(directory).map_err(|error| {
        GuardianOutputError::io("output-directory-read-at", std::io::Error::from(error))
    })?;
    let mut names = Vec::new();
    while let Some(entry) = stream.read() {
        let entry = entry.map_err(|error| {
            GuardianOutputError::io("output-directory-entry-at", std::io::Error::from(error))
        })?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if names.len() >= OUTPUT_MAX_DIRECTORY_ENTRIES_PER_SCAN {
            return Err(GuardianOutputError::FilesystemAuthority(
                "guardian output directory scan bound is exhausted",
            ));
        }
        names
            .try_reserve(1)
            .map_err(|_| GuardianOutputError::Allocation)?;
        names.push(OsStr::from_bytes(bytes).to_os_string());
    }
    Ok(names)
}

fn open_directory_no_follow(path: &Path) -> Result<File, GuardianOutputError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| GuardianOutputError::io("output-directory-open", error))
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn create_private_directory_at(parent: &File, name: &OsStr) -> std::io::Result<()> {
    rustix::fs::mkdirat(parent, name, rustix::fs::Mode::from_raw_mode(0o700))
        .map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn create_private_directory_at(_parent: &File, _name: &OsStr) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "descriptor-relative guardian output directory creation is unsupported on this Unix target",
    ))
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn open_private_directory_at(parent: &File, name: &OsStr) -> std::io::Result<File> {
    rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn open_private_directory_at(_parent: &File, _name: &OsStr) -> std::io::Result<File> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "descriptor-relative guardian output directory open is unsupported on this Unix target",
    ))
}

#[cfg(test)]
fn sync_directory(path: &Path) -> Result<(), GuardianOutputError> {
    open_directory_no_follow(path)?
        .sync_all()
        .map_err(|error| GuardianOutputError::io("output-directory-sync", error))
}

fn validate_normalized_absolute_file_path(path: &Path) -> Result<(), GuardianOutputError> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(GuardianOutputError::InvalidPath);
    }
    Ok(())
}

fn validate_private_directory(path: &Path) -> Result<(), GuardianOutputError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| GuardianOutputError::io("output-private-directory-metadata", error))?;
    validate_private_directory_metadata(&metadata)
}

fn validate_private_directory_metadata(metadata: &Metadata) -> Result<(), GuardianOutputError> {
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "output directories must be current-user mode-0700 real directories",
        ));
    }
    Ok(())
}

fn validate_private_file_metadata(
    metadata: &Metadata,
    expected_len: Option<u64>,
) -> Result<(), GuardianOutputError> {
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || expected_len.is_some_and(|expected| expected != metadata.len())
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "output files must be current-user mode-0600 single-link regular files with the exact expected length",
        ));
    }
    Ok(())
}

fn require_same_identity(
    left: &Metadata,
    right: &Metadata,
    expected_len: Option<u64>,
    site: &'static str,
) -> Result<(), GuardianOutputError> {
    if !FileIdentity::capture(left, expected_len).matches(right) {
        return Err(GuardianOutputError::FilesystemAuthority(site));
    }
    Ok(())
}

fn require_same_directory_identity(
    left: &Metadata,
    right: &Metadata,
    site: &'static str,
) -> Result<(), GuardianOutputError> {
    if !DirectoryIdentity::capture(left).matches(right) {
        return Err(GuardianOutputError::FilesystemAuthority(site));
    }
    Ok(())
}

fn validate_directory_path_identity(
    path: &Path,
    identity: DirectoryIdentity,
) -> Result<(), GuardianOutputError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        GuardianOutputError::io("output-directory-authority-revalidation", error)
    })?;
    if !identity.matches(&metadata) {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output directory identity changed",
        ));
    }
    Ok(())
}

fn checkpoint_catalog_checksum(
    domain: &[u8],
    bytes: &[u8],
) -> [u8; OUTPUT_MANIFEST_CHECKSUM_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    hasher.finalize().into()
}

impl From<&GuardianGenesisReservationIdentityV1> for CheckpointCatalogGenesisReservationBinding {
    fn from(identity: &GuardianGenesisReservationIdentityV1) -> Self {
        Self {
            mux_incarnation: identity.mux_incarnation(),
            spawn_effect_id: identity.spawn_effect_id(),
            durable_pane_id: identity.durable_pane_id(),
            origin_request_id: identity.origin_request_id(),
            spawn_payload_bytes: identity.spawn_payload_bytes(),
            spawn_payload_digest: identity.spawn_payload_digest(),
            spawning_mux_build_identity_digest: identity.spawning_mux_build_identity_digest(),
            live_guardian_build_identity_digest: identity.live_guardian_build_identity_digest(),
            rows: identity.rows(),
            cols: identity.cols(),
            pixel_width: identity.pixel_width(),
            pixel_height: identity.pixel_height(),
            checkpoint_identity_digest: identity.checkpoint_identity_digest(),
            boundary_identity_digest: identity.boundary_identity_digest(),
            upload_id: identity.upload_id(),
        }
    }
}

fn checkpoint_catalog_genesis_candidate_id(
    reservation: CheckpointCatalogGenesisReservationBinding,
) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_CATALOG_GENESIS_CANDIDATE_ID_DOMAIN);
    hasher.update(reservation.mux_incarnation.as_bytes());
    hasher.update(reservation.spawn_effect_id.as_bytes());
    hasher.update(reservation.durable_pane_id.as_bytes());
    hasher.update(reservation.origin_request_id.as_bytes());
    hasher.update(reservation.spawn_payload_bytes.to_le_bytes());
    hasher.update(reservation.spawn_payload_digest);
    hasher.update(reservation.spawning_mux_build_identity_digest);
    hasher.update(reservation.live_guardian_build_identity_digest);
    hasher.update(reservation.rows.to_le_bytes());
    hasher.update(reservation.cols.to_le_bytes());
    hasher.update(reservation.pixel_width.to_le_bytes());
    hasher.update(reservation.pixel_height.to_le_bytes());
    hasher.update(reservation.checkpoint_identity_digest);
    hasher.update(reservation.boundary_identity_digest);
    hasher.update(reservation.upload_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut uuid_bytes = [0_u8; 16];
    uuid_bytes.copy_from_slice(&digest[..16]);
    // Encode a deterministic RFC-4122 variant/version so the canonical path
    // cannot be confused with an arbitrary raw reservation field.
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x50;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(uuid_bytes)
}

fn checkpoint_catalog_candidate_id(
    scope: CheckpointCatalogScope,
    generation: u64,
    predecessor: Option<CheckpointCatalogPredecessor>,
    shape: &CheckpointStageRequestShape,
    seed: &GuardianCheckpointCatalogAdoptionEvidenceSeedV1,
) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_CATALOG_CANDIDATE_ID_DOMAIN);
    hasher.update([scope.tag()]);
    hasher.update(scope.identity().as_bytes());
    hasher.update(generation.to_le_bytes());
    hasher.update([u8::from(predecessor.is_some())]);
    if let Some(predecessor) = predecessor {
        hasher.update(predecessor.generation.to_le_bytes());
        hasher.update(predecessor.candidate_id.as_bytes());
        hasher.update(predecessor.candidate_checksum);
        hasher.update(predecessor.checkpoint_id);
        hasher.update(predecessor.boundary_id);
    }
    hasher.update(shape.upload_id.as_bytes());
    hasher.update(shape.descriptor.checkpoint_id().into_bytes());
    hasher.update(shape.descriptor.boundary_id().into_bytes());
    hasher.update(shape.descriptor.terminal_payload_digest());
    hasher.update(shape.descriptor.capture_generation().to_le_bytes());
    hasher.update(shape.descriptor.replay_semantics_id());
    hasher.update(shape.descriptor.rows().to_le_bytes());
    hasher.update(shape.descriptor.cols().to_le_bytes());
    hasher.update(shape.chunk_bytes.to_le_bytes());
    hasher.update(shape.total_chunks.to_le_bytes());
    hasher.update(shape.total_bytes.to_le_bytes());
    hasher.update(seed.pane_id().as_bytes());
    hasher.update(seed.mux_incarnation().as_bytes());
    hasher.update(seed.canonical_request_id().as_bytes());
    hasher.update(seed.generation().to_le_bytes());
    hasher.update(seed.sequence().to_le_bytes());
    hasher.update(seed.effect_id().as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut uuid_bytes = [0_u8; 16];
    uuid_bytes.copy_from_slice(&digest[..16]);
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x50;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(uuid_bytes)
}

fn checkpoint_catalog_genesis_metadata_matches_reservation(
    metadata: &CheckpointCatalogMetadata,
    reservation: CheckpointCatalogGenesisReservationBinding,
) -> bool {
    metadata.identity.scope
        == (CheckpointCatalogScope::Genesis {
            spawn_effect_id: reservation.spawn_effect_id,
        })
        && metadata.identity.generation == 1
        && metadata.identity.candidate_id == checkpoint_catalog_genesis_candidate_id(reservation)
        && metadata.predecessor.is_none()
        && metadata.upload_id == reservation.upload_id
        && (metadata.checkpoint_id, metadata.boundary_id)
            == (
                reservation.checkpoint_identity_digest,
                reservation.boundary_identity_digest,
            )
        && metadata.capture_generation == 1
        && metadata.rows == u32::from(reservation.rows)
        && metadata.cols == u32::from(reservation.cols)
        && metadata.adoption_mux_incarnation == reservation.mux_incarnation
        && metadata.adoption_effect_id == reservation.spawn_effect_id
        && metadata.adoption_sequence == 0
        && metadata.genesis_durable_pane_id == reservation.durable_pane_id
        && metadata.genesis_origin_request_id == reservation.origin_request_id
        && metadata.genesis_spawn_payload_bytes == reservation.spawn_payload_bytes
        && metadata.genesis_spawn_payload_digest == reservation.spawn_payload_digest
        && metadata.genesis_spawning_mux_build_identity_digest
            == reservation.spawning_mux_build_identity_digest
        && metadata.genesis_live_guardian_build_identity_digest
            == reservation.live_guardian_build_identity_digest
        && metadata.genesis_pixel_width == reservation.pixel_width
        && metadata.genesis_pixel_height == reservation.pixel_height
}

fn checkpoint_catalog_scope_from_wire(
    tag: u8,
    identity: Uuid,
) -> Result<CheckpointCatalogScope, GuardianCheckpointStageStoreError> {
    if identity.is_nil() {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    match tag {
        1 => Ok(CheckpointCatalogScope::Pane { pane_id: identity }),
        2 => Ok(CheckpointCatalogScope::Genesis {
            spawn_effect_id: identity,
        }),
        _ => Err(GuardianCheckpointStageStoreError::Poisoned),
    }
}

fn checkpoint_catalog_validate_metadata(
    metadata: &CheckpointCatalogMetadata,
) -> Result<(), GuardianCheckpointStageStoreError> {
    let predecessor_valid = match metadata.predecessor {
        None => metadata.identity.generation == 1,
        Some(predecessor) => {
            predecessor.generation > 0
                && predecessor.generation.checked_add(1) == Some(metadata.identity.generation)
                && !predecessor.candidate_id.is_nil()
                && predecessor.candidate_checksum != [0; OUTPUT_MANIFEST_CHECKSUM_BYTES]
                && predecessor.checkpoint_id != [0; 32]
                && predecessor.boundary_id != [0; 32]
        }
    };
    let scope_binding_valid = match metadata.identity.scope {
        CheckpointCatalogScope::Pane { .. } => {
            metadata.adoption_sequence > 0
                && metadata.genesis_durable_pane_id.is_nil()
                && metadata.genesis_origin_request_id.is_nil()
                && metadata.genesis_spawn_payload_bytes == 0
                && metadata.genesis_spawn_payload_digest == [0; 32]
                && metadata.genesis_spawning_mux_build_identity_digest == [0; 32]
                && metadata.genesis_live_guardian_build_identity_digest == [0; 32]
                && metadata.genesis_pixel_width == 0
                && metadata.genesis_pixel_height == 0
        }
        CheckpointCatalogScope::Genesis { spawn_effect_id } => {
            metadata.identity.generation == 1
                && metadata.predecessor.is_none()
                && metadata.capture_generation == 1
                && metadata.adoption_effect_id == spawn_effect_id
                && metadata.adoption_sequence == 0
                && !metadata.genesis_durable_pane_id.is_nil()
                && !metadata.genesis_origin_request_id.is_nil()
                && metadata.genesis_spawn_payload_bytes > 0
                && metadata.genesis_spawn_payload_digest != [0; 32]
                && metadata.genesis_spawning_mux_build_identity_digest != [0; 32]
                && metadata.genesis_live_guardian_build_identity_digest != [0; 32]
                && u16::try_from(metadata.rows).is_ok()
                && u16::try_from(metadata.cols).is_ok()
        }
    };
    if metadata.identity.scope.identity().is_nil()
        || metadata.identity.generation == 0
        || metadata.identity.candidate_id.is_nil()
        || metadata.upload_id.is_nil()
        || metadata.completion_id.is_nil()
        || metadata.checkpoint_id == [0; 32]
        || metadata.boundary_id == [0; 32]
        || metadata.terminal_payload_digest == [0; 32]
        || metadata.total_bytes == 0
        || metadata.total_bytes > GUARDIAN_MAX_CHECKPOINT_BYTES
        || metadata.chunk_count == 0
        || metadata.chunk_count > GUARDIAN_MAX_CHECKPOINT_CHUNKS
        || metadata.capture_generation == 0
        || metadata.replay_semantics_id == [0; 32]
        || metadata.rows == 0
        || metadata.cols == 0
        || metadata.adoption_mux_incarnation.is_nil()
        || metadata.adoption_effect_id.is_nil()
        || !predecessor_valid
        || !scope_binding_valid
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    Ok(())
}

fn checkpoint_catalog_encode_candidate_base(
    candidate: &mut CheckpointCatalogCandidate,
) -> Result<Vec<u8>, GuardianCheckpointStageStoreError> {
    if candidate.format != CheckpointCatalogFormat::ProtectedV3 {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    checkpoint_catalog_validate_metadata(&candidate.metadata)?;
    let expected_records = usize::try_from(candidate.metadata.chunk_count)
        .ok()
        .and_then(|chunks| chunks.checked_add(2))
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    if candidate.records.len() != expected_records {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let record_bytes = candidate
        .records
        .iter()
        .try_fold(0_usize, |total, record| {
            total
                .checked_add(GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES)
                .and_then(|bytes| bytes.checked_add(record.ciphertext_bytes()))
                .ok_or(GuardianCheckpointStageStoreError::Capacity)
        })?;
    let total_bytes = CHECKPOINT_CATALOG_HEADER_BYTES
        .checked_add(record_bytes)
        .and_then(|bytes| bytes.checked_add(OUTPUT_MANIFEST_CHECKSUM_BYTES))
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    if u64::try_from(total_bytes)
        .ok()
        .is_none_or(|bytes| bytes == 0 || bytes > CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES)
    {
        return Err(GuardianCheckpointStageStoreError::Capacity);
    }
    let metadata = candidate.metadata;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(total_bytes)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    encoded.extend_from_slice(&candidate.format.candidate_magic());
    encoded.extend_from_slice(&candidate.format.version().to_le_bytes());
    encoded.push(metadata.identity.scope.tag());
    encoded.push(u8::from(metadata.predecessor.is_some()));
    encoded.extend_from_slice(&[0; 2]);
    encoded.extend_from_slice(metadata.identity.scope.identity().as_bytes());
    encoded.extend_from_slice(&metadata.identity.generation.to_le_bytes());
    encoded.extend_from_slice(metadata.identity.candidate_id.as_bytes());
    encoded.extend_from_slice(metadata.upload_id.as_bytes());
    encoded.extend_from_slice(metadata.completion_id.as_bytes());
    encoded.extend_from_slice(&metadata.checkpoint_id);
    encoded.extend_from_slice(&metadata.boundary_id);
    encoded.extend_from_slice(&metadata.terminal_payload_digest);
    encoded.extend_from_slice(&metadata.total_bytes.to_le_bytes());
    encoded.extend_from_slice(&metadata.chunk_count.to_le_bytes());
    encoded.extend_from_slice(
        &u32::try_from(candidate.records.len())
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(
        &u64::try_from(record_bytes)
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(&metadata.capture_generation.to_le_bytes());
    encoded.extend_from_slice(&metadata.replay_semantics_id);
    encoded.extend_from_slice(&metadata.rows.to_le_bytes());
    encoded.extend_from_slice(&metadata.cols.to_le_bytes());
    encoded.extend_from_slice(metadata.adoption_mux_incarnation.as_bytes());
    encoded.extend_from_slice(metadata.adoption_effect_id.as_bytes());
    encoded.extend_from_slice(&metadata.adoption_sequence.to_le_bytes());
    encoded.extend_from_slice(metadata.genesis_durable_pane_id.as_bytes());
    encoded.extend_from_slice(metadata.genesis_origin_request_id.as_bytes());
    encoded.extend_from_slice(&metadata.genesis_spawn_payload_bytes.to_le_bytes());
    encoded.extend_from_slice(&metadata.genesis_spawn_payload_digest);
    encoded.extend_from_slice(&metadata.genesis_spawning_mux_build_identity_digest);
    encoded.extend_from_slice(&metadata.genesis_live_guardian_build_identity_digest);
    encoded.extend_from_slice(&metadata.genesis_pixel_width.to_le_bytes());
    encoded.extend_from_slice(&metadata.genesis_pixel_height.to_le_bytes());
    if let Some(predecessor) = metadata.predecessor {
        encoded.extend_from_slice(&predecessor.generation.to_le_bytes());
        encoded.extend_from_slice(predecessor.candidate_id.as_bytes());
        encoded.extend_from_slice(&predecessor.candidate_checksum);
        encoded.extend_from_slice(&predecessor.checkpoint_id);
        encoded.extend_from_slice(&predecessor.boundary_id);
    } else {
        encoded.extend_from_slice(&[0; 120]);
    }
    let evidence_record_bytes = match metadata.identity.scope {
        CheckpointCatalogScope::Pane { .. } => {
            u32::try_from(CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_RECORD_BYTES)
                .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?
        }
        CheckpointCatalogScope::Genesis { .. } => 0,
    };
    encoded.extend_from_slice(&evidence_record_bytes.to_le_bytes());
    encoded.extend_from_slice(&[0; 8]);
    if encoded.len() != CHECKPOINT_CATALOG_HEADER_BYTES {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    for record in &candidate.records {
        encoded.extend_from_slice(&record.fixed_header());
        encoded.extend_from_slice(record.ciphertext());
    }
    candidate.checksum =
        checkpoint_catalog_checksum(candidate.format.candidate_checksum_domain(), &encoded);
    encoded.extend_from_slice(&candidate.checksum);
    if encoded.len() != total_bytes {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    Ok(encoded)
}

fn checkpoint_catalog_encode_candidate(
    candidate: &mut CheckpointCatalogCandidate,
) -> Result<Vec<u8>, GuardianCheckpointStageStoreError> {
    let mut encoded = checkpoint_catalog_encode_candidate_base(candidate)?;
    match (
        candidate.metadata.identity.scope,
        &candidate.adoption_evidence,
    ) {
        (CheckpointCatalogScope::Pane { .. }, Some(evidence)) => {
            let evidence_bytes = GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES
                .checked_add(evidence.ciphertext_bytes())
                .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
            if u64::try_from(evidence_bytes).ok()
                != Some(CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_RECORD_BYTES)
                || evidence.plaintext_bytes() != GUARDIAN_CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_BYTES
            {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            encoded
                .try_reserve_exact(evidence_bytes)
                .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
            debug_assert!(
                encoded.capacity().saturating_sub(encoded.len()) >= evidence_bytes,
                "fallible reserve must cover the complete fixed evidence record"
            );
            encoded.extend_from_slice(&evidence.fixed_header());
            encoded.extend_from_slice(evidence.ciphertext());
        }
        (CheckpointCatalogScope::Genesis { .. }, None) => {}
        _ => return Err(GuardianCheckpointStageStoreError::Poisoned),
    }
    if u64::try_from(encoded.len())
        .ok()
        .is_none_or(|bytes| bytes == 0 || bytes > CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES)
    {
        return Err(GuardianCheckpointStageStoreError::Capacity);
    }
    Ok(encoded)
}

fn checkpoint_catalog_decode_candidate(
    bytes: &[u8],
) -> Result<CheckpointCatalogCandidate, GuardianCheckpointStageStoreError> {
    if bytes.len() < CHECKPOINT_CATALOG_HEADER_BYTES + OUTPUT_MANIFEST_CHECKSUM_BYTES
        || u64::try_from(bytes.len())
            .ok()
            .is_none_or(|length| length > CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES)
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let mut decoder = ManifestDecoder::new(&bytes[..CHECKPOINT_CATALOG_HEADER_BYTES]);
    let format =
        CheckpointCatalogFormat::from_candidate_header(decoder.take::<8>()?, decoder.u32()?)?;
    let scope_tag = decoder.take::<1>()?[0];
    let predecessor_present = decoder.take::<1>()?[0];
    if decoder.take::<2>()? != [0; 2] {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let scope = checkpoint_catalog_scope_from_wire(scope_tag, decoder.uuid()?)?;
    let generation = decoder.u64()?;
    let candidate_id = decoder.uuid()?;
    let upload_id = decoder.uuid()?;
    let completion_id = decoder.uuid()?;
    let checkpoint_id = decoder.take::<32>()?;
    let boundary_id = decoder.take::<32>()?;
    let terminal_payload_digest = decoder.take::<32>()?;
    let total_bytes = decoder.u64()?;
    let chunk_count = decoder.u32()?;
    let record_count = decoder.u32()?;
    let record_bytes =
        usize::try_from(decoder.u64()?).map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
    let capture_generation = decoder.u64()?;
    let replay_semantics_id = decoder.take::<32>()?;
    let rows = decoder.u32()?;
    let cols = decoder.u32()?;
    let adoption_mux_incarnation = decoder.uuid()?;
    let adoption_effect_id = decoder.uuid()?;
    let adoption_sequence = decoder.u64()?;
    let genesis_durable_pane_id = decoder.uuid()?;
    let genesis_origin_request_id = decoder.uuid()?;
    let genesis_spawn_payload_bytes = decoder.u64()?;
    let genesis_spawn_payload_digest = decoder.take::<32>()?;
    let genesis_spawning_mux_build_identity_digest = decoder.take::<32>()?;
    let genesis_live_guardian_build_identity_digest = decoder.take::<32>()?;
    let genesis_pixel_width = u16::from_le_bytes(decoder.take::<2>()?);
    let genesis_pixel_height = u16::from_le_bytes(decoder.take::<2>()?);
    let predecessor_generation = decoder.u64()?;
    let predecessor_candidate_id = decoder.uuid()?;
    let predecessor_checksum = decoder.take::<32>()?;
    let predecessor_checkpoint_id = decoder.take::<32>()?;
    let predecessor_boundary_id = decoder.take::<32>()?;
    let evidence_record_bytes = match format {
        CheckpointCatalogFormat::LegacyV2 => {
            if decoder.take::<12>()? != [0; 12] {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            0
        }
        CheckpointCatalogFormat::ProtectedV3 => {
            let evidence_record_bytes = usize::try_from(decoder.u32()?)
                .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
            if decoder.take::<8>()? != [0; 8] {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            evidence_record_bytes
        }
    };
    if decoder.offset != CHECKPOINT_CATALOG_HEADER_BYTES {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let expected_evidence_record_bytes = match (format, scope) {
        (CheckpointCatalogFormat::ProtectedV3, CheckpointCatalogScope::Pane { .. }) => {
            usize::try_from(CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_RECORD_BYTES)
                .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?
        }
        (
            CheckpointCatalogFormat::LegacyV2,
            CheckpointCatalogScope::Pane { .. } | CheckpointCatalogScope::Genesis { .. },
        )
        | (CheckpointCatalogFormat::ProtectedV3, CheckpointCatalogScope::Genesis { .. }) => 0,
    };
    let checksum_offset = CHECKPOINT_CATALOG_HEADER_BYTES
        .checked_add(record_bytes)
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    let evidence_offset = checksum_offset
        .checked_add(OUTPUT_MANIFEST_CHECKSUM_BYTES)
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    if evidence_record_bytes != expected_evidence_record_bytes
        || evidence_offset.checked_add(evidence_record_bytes) != Some(bytes.len())
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let mut checksum = [0; OUTPUT_MANIFEST_CHECKSUM_BYTES];
    checksum.copy_from_slice(&bytes[checksum_offset..evidence_offset]);
    if !checkpoint_bytes_match(
        &checksum,
        &checkpoint_catalog_checksum(
            format.candidate_checksum_domain(),
            &bytes[..checksum_offset],
        ),
    ) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let predecessor = match predecessor_present {
        0 if predecessor_generation == 0
            && predecessor_candidate_id.is_nil()
            && predecessor_checksum == [0; 32]
            && predecessor_checkpoint_id == [0; 32]
            && predecessor_boundary_id == [0; 32] =>
        {
            None
        }
        1 => Some(CheckpointCatalogPredecessor {
            generation: predecessor_generation,
            candidate_id: predecessor_candidate_id,
            candidate_checksum: predecessor_checksum,
            checkpoint_id: predecessor_checkpoint_id,
            boundary_id: predecessor_boundary_id,
        }),
        _ => return Err(GuardianCheckpointStageStoreError::Poisoned),
    };
    let metadata = CheckpointCatalogMetadata {
        identity: CheckpointCatalogIdentity {
            scope,
            generation,
            candidate_id,
        },
        predecessor,
        upload_id,
        completion_id,
        checkpoint_id,
        boundary_id,
        terminal_payload_digest,
        total_bytes,
        chunk_count,
        capture_generation,
        replay_semantics_id,
        rows,
        cols,
        adoption_mux_incarnation,
        adoption_effect_id,
        adoption_sequence,
        genesis_durable_pane_id,
        genesis_origin_request_id,
        genesis_spawn_payload_bytes,
        genesis_spawn_payload_digest,
        genesis_spawning_mux_build_identity_digest,
        genesis_live_guardian_build_identity_digest,
        genesis_pixel_width,
        genesis_pixel_height,
    };
    checkpoint_catalog_validate_metadata(&metadata)?;
    let expected_records = chunk_count
        .checked_add(2)
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    if record_count != expected_records {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let record_count =
        usize::try_from(record_count).map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(record_count)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    let mut offset = CHECKPOINT_CATALOG_HEADER_BYTES;
    for _ in 0..record_count {
        let header_end = offset
            .checked_add(GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES)
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        let header = bytes
            .get(offset..header_end)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let ciphertext_bytes =
            GuardianEncryptedCheckpointStageRecordV1::persisted_ciphertext_bytes(
                header,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            )
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
        let record_end = header_end
            .checked_add(ciphertext_bytes)
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        if record_end > checksum_offset {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        let mut ciphertext = Vec::new();
        ciphertext
            .try_reserve_exact(ciphertext_bytes)
            .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
        ciphertext.extend_from_slice(&bytes[header_end..record_end]);
        records.push(
            GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                header,
                ciphertext,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            )
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?,
        );
        offset = record_end;
    }
    if offset != checksum_offset {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let adoption_evidence = if evidence_record_bytes == 0 {
        None
    } else {
        let header_end = evidence_offset
            .checked_add(GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES)
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        let header = bytes
            .get(evidence_offset..header_end)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let ciphertext_bytes =
            GuardianEncryptedCheckpointStageRecordV1::persisted_ciphertext_bytes(
                header,
                GUARDIAN_CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_BYTES,
            )
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
        if header_end.checked_add(ciphertext_bytes) != Some(bytes.len()) {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        let mut ciphertext = Vec::new();
        ciphertext
            .try_reserve_exact(ciphertext_bytes)
            .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
        ciphertext.extend_from_slice(&bytes[header_end..]);
        Some(
            GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                header,
                ciphertext,
                GUARDIAN_CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_BYTES,
            )
            .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?,
        )
    };
    Ok(CheckpointCatalogCandidate {
        format,
        metadata,
        records,
        checksum,
        adoption_evidence,
    })
}

fn checkpoint_catalog_marker_for_candidate(
    candidate: &CheckpointCatalogCandidate,
) -> CheckpointCatalogMarker {
    let (predecessor_generation, predecessor_candidate_id, predecessor_checksum) = candidate
        .metadata
        .predecessor
        .map_or((None, Uuid::nil(), [0; 32]), |predecessor| {
            (
                Some(predecessor.generation),
                predecessor.candidate_id,
                predecessor.candidate_checksum,
            )
        });
    CheckpointCatalogMarker {
        format: candidate.format,
        identity: candidate.metadata.identity,
        predecessor_generation,
        predecessor_candidate_id,
        predecessor_checksum,
        upload_id: candidate.metadata.upload_id,
        completion_id: candidate.metadata.completion_id,
        checkpoint_id: candidate.metadata.checkpoint_id,
        boundary_id: candidate.metadata.boundary_id,
        terminal_payload_digest: candidate.metadata.terminal_payload_digest,
        candidate_checksum: candidate.checksum,
        adoption_mux_incarnation: candidate.metadata.adoption_mux_incarnation,
        adoption_effect_id: candidate.metadata.adoption_effect_id,
        adoption_sequence: candidate.metadata.adoption_sequence,
    }
}

fn checkpoint_catalog_encode_marker(marker: &CheckpointCatalogMarker) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(CHECKPOINT_CATALOG_MARKER_BYTES);
    encoded.extend_from_slice(&marker.format.marker_magic());
    encoded.extend_from_slice(&marker.format.version().to_le_bytes());
    encoded.push(marker.identity.scope.tag());
    encoded.push(u8::from(marker.predecessor_generation.is_some()));
    encoded.extend_from_slice(&[0; 2]);
    encoded.extend_from_slice(marker.identity.scope.identity().as_bytes());
    encoded.extend_from_slice(&marker.identity.generation.to_le_bytes());
    encoded.extend_from_slice(marker.identity.candidate_id.as_bytes());
    encoded.extend_from_slice(&marker.candidate_checksum);
    encoded.extend_from_slice(&marker.checkpoint_id);
    encoded.extend_from_slice(&marker.boundary_id);
    encoded.extend_from_slice(&marker.terminal_payload_digest);
    encoded.extend_from_slice(marker.upload_id.as_bytes());
    encoded.extend_from_slice(marker.completion_id.as_bytes());
    encoded.extend_from_slice(&marker.predecessor_generation.unwrap_or(0).to_le_bytes());
    encoded.extend_from_slice(marker.predecessor_candidate_id.as_bytes());
    encoded.extend_from_slice(&marker.predecessor_checksum);
    encoded.extend_from_slice(marker.adoption_mux_incarnation.as_bytes());
    encoded.extend_from_slice(marker.adoption_effect_id.as_bytes());
    encoded.extend_from_slice(&marker.adoption_sequence.to_le_bytes());
    debug_assert_eq!(encoded.len(), CHECKPOINT_CATALOG_MARKER_BODY_BYTES);
    let checksum = checkpoint_catalog_checksum(marker.format.marker_checksum_domain(), &encoded);
    encoded.extend_from_slice(&checksum);
    encoded
}

fn checkpoint_catalog_decode_marker(
    bytes: &[u8],
) -> Result<CheckpointCatalogMarker, GuardianCheckpointStageStoreError> {
    if bytes.len() != CHECKPOINT_CATALOG_MARKER_BYTES {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let mut decoder = ManifestDecoder::new(&bytes[..CHECKPOINT_CATALOG_MARKER_BODY_BYTES]);
    let format = CheckpointCatalogFormat::from_marker_header(decoder.take::<8>()?, decoder.u32()?)?;
    let expected = checkpoint_catalog_checksum(
        format.marker_checksum_domain(),
        &bytes[..CHECKPOINT_CATALOG_MARKER_BODY_BYTES],
    );
    if !checkpoint_bytes_match(&expected, &bytes[CHECKPOINT_CATALOG_MARKER_BODY_BYTES..]) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let scope_tag = decoder.take::<1>()?[0];
    let predecessor_present = decoder.take::<1>()?[0];
    if decoder.take::<2>()? != [0; 2] {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let scope = checkpoint_catalog_scope_from_wire(scope_tag, decoder.uuid()?)?;
    let identity = CheckpointCatalogIdentity {
        scope,
        generation: decoder.u64()?,
        candidate_id: decoder.uuid()?,
    };
    let candidate_checksum = decoder.take::<32>()?;
    let checkpoint_id = decoder.take::<32>()?;
    let boundary_id = decoder.take::<32>()?;
    let terminal_payload_digest = decoder.take::<32>()?;
    let upload_id = decoder.uuid()?;
    let completion_id = decoder.uuid()?;
    let predecessor_generation = decoder.u64()?;
    let predecessor_candidate_id = decoder.uuid()?;
    let predecessor_checksum = decoder.take::<32>()?;
    let adoption_mux_incarnation = decoder.uuid()?;
    let adoption_effect_id = decoder.uuid()?;
    let adoption_sequence = decoder.u64()?;
    if decoder.offset != CHECKPOINT_CATALOG_MARKER_BODY_BYTES
        || identity.generation == 0
        || identity.candidate_id.is_nil()
        || upload_id.is_nil()
        || completion_id.is_nil()
        || candidate_checksum == [0; 32]
        || checkpoint_id == [0; 32]
        || boundary_id == [0; 32]
        || terminal_payload_digest == [0; 32]
        || adoption_mux_incarnation.is_nil()
        || adoption_effect_id.is_nil()
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let predecessor_generation = match predecessor_present {
        0 if predecessor_generation == 0
            && predecessor_candidate_id.is_nil()
            && predecessor_checksum == [0; 32] =>
        {
            None
        }
        1 if predecessor_generation > 0
            && !predecessor_candidate_id.is_nil()
            && predecessor_checksum != [0; 32] =>
        {
            Some(predecessor_generation)
        }
        _ => return Err(GuardianCheckpointStageStoreError::Poisoned),
    };
    let scope_binding_valid = match identity.scope {
        CheckpointCatalogScope::Pane { .. } => adoption_sequence > 0,
        CheckpointCatalogScope::Genesis { spawn_effect_id } => {
            identity.generation == 1
                && predecessor_generation.is_none()
                && adoption_effect_id == spawn_effect_id
                && adoption_sequence == 0
        }
    };
    if !scope_binding_valid {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    Ok(CheckpointCatalogMarker {
        format,
        identity,
        predecessor_generation,
        predecessor_candidate_id,
        predecessor_checksum,
        upload_id,
        completion_id,
        checkpoint_id,
        boundary_id,
        terminal_payload_digest,
        candidate_checksum,
        adoption_mux_incarnation,
        adoption_effect_id,
        adoption_sequence,
    })
}

fn checkpoint_catalog_read_file(
    inner: &GuardianCheckpointStageStoreInner,
    path: &Path,
    expected_identity: FileIdentity,
    maximum_bytes: u64,
) -> Result<Vec<u8>, GuardianCheckpointStageStoreError> {
    let mut file = open_private_file_at(&inner.directory, &inner.directory_path, path, false)?;
    let metadata_before = file.metadata().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-catalog-open-metadata", error)
    })?;
    validate_private_file_metadata(&metadata_before, expected_identity.expected_len)?;
    if !expected_identity.matches(&metadata_before)
        || metadata_before.len() == 0
        || metadata_before.len() > maximum_bytes
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let length = usize::try_from(metadata_before.len())
        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    bytes.resize(length, 0);
    file.read_exact(&mut bytes).map_err(|error| {
        if error.kind() == ErrorKind::UnexpectedEof {
            GuardianCheckpointStageStoreError::Poisoned
        } else {
            GuardianCheckpointStageStoreError::io("checkpoint-catalog-read", error)
        }
    })?;
    let metadata_after = file.metadata().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-catalog-final-metadata", error)
    })?;
    if !expected_identity.matches(&metadata_after) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        path,
        expected_identity,
    )?;
    Ok(bytes)
}

fn checkpoint_catalog_validate_candidate_records(
    inner: &GuardianCheckpointStageStoreInner,
    candidate: &CheckpointCatalogCandidate,
) -> Result<(), GuardianCheckpointStageStoreError> {
    let metadata = candidate.metadata;
    checkpoint_catalog_validate_metadata(&metadata)?;
    let expected_records = usize::try_from(metadata.chunk_count)
        .ok()
        .and_then(|chunks| chunks.checked_add(2))
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    if candidate.records.len() != expected_records {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let candidate_record = candidate
        .records
        .first()
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    let candidate_context = candidate_record.context();
    if candidate_context.kind() != GuardianCheckpointStageRecordKindV1::CandidateMetadata
        || candidate_context.publication_id() != metadata.completion_id
        || candidate_context.plaintext_bytes()
            != u32::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES)
                .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let begin_plaintext = inner.cipher.open(
        &candidate_context,
        candidate_record,
        u32::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES)
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
    )?;
    let begin_request = GuardianCheckpointStageRequestV1::decode(&begin_plaintext)?;
    if begin_request.kind() != GuardianCheckpointStageKindV1::Begin {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let shape = CheckpointStageRequestShape::from_request(&begin_request)?;
    let canonical_begin = shape.begin_payload()?;
    let total_bytes_match = shape.total_bytes == metadata.total_bytes;
    let chunk_count_matches = shape.total_chunks == metadata.chunk_count;
    let descriptor_geometry_matches =
        shape.descriptor.rows() == metadata.rows && shape.descriptor.cols() == metadata.cols;
    if !total_bytes_match
        || !chunk_count_matches
        || !descriptor_geometry_matches
        || !checkpoint_bytes_match(&canonical_begin, &begin_plaintext)
        || CheckpointCatalogScope::from_stage_scope(shape.path_scope) != metadata.identity.scope
        || shape.upload_id != metadata.upload_id
        || shape.descriptor.capture_generation() != metadata.capture_generation
        || shape.descriptor.checkpoint_id().into_bytes() != metadata.checkpoint_id
        || shape.descriptor.boundary_id().into_bytes() != metadata.boundary_id
        || shape.descriptor.terminal_payload_digest() != metadata.terminal_payload_digest
        || shape.descriptor.replay_semantics_id() != metadata.replay_semantics_id
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let candidate_identity =
        GuardianCheckpointCandidateIdentityV1::from_canonical_begin_plaintext(&canonical_begin)?;
    let expected_candidate = GuardianCheckpointStageSealIntentV1::candidate_metadata(
        &shape.binding,
        shape.upload_id,
        metadata.completion_id,
        canonical_begin,
    )?;
    if !checkpoint_context_public_identity_matches(
        &candidate_context,
        &expected_candidate.context(),
    ) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }

    let payload_bytes = usize::try_from(metadata.total_bytes)
        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
    let mut payload = Zeroizing::new(Vec::new());
    payload
        .try_reserve_exact(payload_bytes)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    let mut chunk_set = GuardianCheckpointOrderedChunkSetBuilderV1::new(
        metadata.total_bytes,
        shape.chunk_bytes,
        metadata.chunk_count,
    )?;
    for index in 0..metadata.chunk_count {
        let record_index = usize::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        let record = candidate
            .records
            .get(record_index)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let context = record.context();
        let offset = u64::from(index)
            .checked_mul(u64::from(shape.chunk_bytes))
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        let expected_bytes = metadata
            .total_bytes
            .checked_sub(offset)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?
            .min(u64::from(shape.chunk_bytes));
        let expected_bytes_u32 = u32::try_from(expected_bytes)
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
        let chunk = inner.cipher.open(&context, record, expected_bytes_u32)?;
        if u64::try_from(chunk.len()).ok() != Some(expected_bytes) {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        chunk_set.push_authenticated_chunk(index, offset, &chunk)?;
        payload.extend_from_slice(&chunk);
        let expected = GuardianCheckpointStageSealIntentV1::chunk(
            &shape.binding,
            shape.upload_id,
            metadata.completion_id,
            index,
            offset,
            chunk,
        )?;
        if !checkpoint_context_public_identity_matches(&context, &expected.context()) {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
    }
    if payload.len() != payload_bytes {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    shape.descriptor.validate_canonical_payload(&payload)?;
    let ordered_chunk_set_identity = chunk_set.finish()?;
    let seal_request = GuardianCheckpointStageRequestV1::seal(
        shape.scope,
        shape.upload_id,
        shape.descriptor,
        shape.chunk_bytes,
    )?;
    let seal_record = candidate
        .records
        .last()
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    if seal_record.context().kind() != GuardianCheckpointStageRecordKindV1::SealManifest {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let _completion_receipt = inner.cipher.inspect_durable_manifest_receipt(
        &shape.binding,
        seal_request,
        metadata.completion_id,
        candidate_identity,
        ordered_chunk_set_identity,
        seal_record,
    )?;
    Ok(())
}

fn checkpoint_catalog_adoption_binding(
    candidate: &CheckpointCatalogCandidate,
) -> Result<GuardianCheckpointCatalogAdoptionBindingV1, GuardianCheckpointStageStoreError> {
    let metadata = candidate.metadata;
    let CheckpointCatalogScope::Pane { pane_id } = metadata.identity.scope else {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    };
    let predecessor = metadata
        .predecessor
        .map(|predecessor| {
            GuardianCheckpointCatalogPredecessorBindingV1::new(
                predecessor.generation,
                predecessor.candidate_id,
                predecessor.candidate_checksum,
                predecessor.checkpoint_id,
                predecessor.boundary_id,
            )
        })
        .transpose()?;
    GuardianCheckpointCatalogAdoptionBindingV1::from_catalog_parts(
        pane_id,
        metadata.capture_generation,
        metadata.adoption_mux_incarnation,
        metadata.adoption_effect_id,
        metadata.adoption_sequence,
        metadata.identity.generation,
        metadata.identity.candidate_id,
        candidate.checksum,
        predecessor,
        metadata.upload_id,
        metadata.completion_id,
        metadata.checkpoint_id,
        metadata.boundary_id,
        metadata.terminal_payload_digest,
        metadata.total_bytes,
        metadata.chunk_count,
        metadata.replay_semantics_id,
        metadata.rows,
        metadata.cols,
    )
    .map_err(Into::into)
}

fn checkpoint_catalog_recover_adoption_evidence(
    inner: &GuardianCheckpointStageStoreInner,
    candidate: &CheckpointCatalogCandidate,
) -> Result<GuardianCheckpointCatalogAdoptionEvidenceV1, GuardianCheckpointStageStoreError> {
    let binding = checkpoint_catalog_adoption_binding(candidate)?;
    let evidence = candidate
        .adoption_evidence
        .as_ref()
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    inner
        .cipher
        .inspect_catalog_adoption_evidence(&binding, evidence)
        .map_err(Into::into)
}

fn checkpoint_catalog_recover_adoption_evidence_with_seed(
    inner: &GuardianCheckpointStageStoreInner,
    candidate: &CheckpointCatalogCandidate,
    seed: GuardianCheckpointCatalogAdoptionEvidenceSeedV1,
) -> Result<GuardianCheckpointCatalogAdoptionEvidenceV1, GuardianCheckpointStageStoreError> {
    let binding = checkpoint_catalog_adoption_binding(candidate)?;
    let evidence = candidate
        .adoption_evidence
        .as_ref()
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    inner
        .cipher
        .inspect_catalog_adoption_evidence_with_seed(seed, &binding, evidence)
        .map_err(Into::into)
}

fn checkpoint_catalog_seal_adoption_evidence(
    inner: &GuardianCheckpointStageStoreInner,
    candidate: &CheckpointCatalogCandidate,
    seed: GuardianCheckpointCatalogAdoptionEvidenceSeedV1,
) -> Result<GuardianEncryptedCheckpointStageRecordV1, GuardianCheckpointStageStoreError> {
    let binding = checkpoint_catalog_adoption_binding(candidate)?;
    inner
        .cipher
        .seal_catalog_adoption_evidence(seed, &binding)
        .map_err(Into::into)
}

fn checkpoint_catalog_marker_matches_candidate(
    marker: &CheckpointCatalogMarker,
    candidate: &CheckpointCatalogCandidate,
) -> bool {
    let expected = checkpoint_catalog_marker_for_candidate(candidate);
    *marker == expected
}

fn checkpoint_catalog_validate_chain(
    published: &mut Vec<PublishedCheckpointCatalogMember>,
) -> Result<(), GuardianCheckpointStageStoreError> {
    published.sort_by_key(|member| member.metadata.identity.generation);
    let expected_scope = published
        .first()
        .map(|member| member.metadata.identity.scope);
    let mut previous: Option<&PublishedCheckpointCatalogMember> = None;
    let mut boundaries = BTreeMap::new();
    let mut checkpoints = BTreeSet::new();
    let mut adoption_effects = BTreeSet::new();
    for member in published.iter() {
        checkpoint_catalog_validate_metadata(&member.metadata)?;
        let expected_predecessor = previous.map(|prior| CheckpointCatalogPredecessor {
            generation: prior.metadata.identity.generation,
            candidate_id: prior.metadata.identity.candidate_id,
            candidate_checksum: prior.candidate_checksum,
            checkpoint_id: prior.metadata.checkpoint_id,
            boundary_id: prior.metadata.boundary_id,
        });
        if Some(member.metadata.identity.scope) != expected_scope
            || member.metadata.predecessor != expected_predecessor
            || previous.is_none() != (member.metadata.identity.generation == 1)
            || previous.is_some_and(|prior| {
                prior.format == CheckpointCatalogFormat::ProtectedV3
                    && member.format == CheckpointCatalogFormat::LegacyV2
            })
            || previous.is_some_and(|prior| {
                prior.metadata.identity.generation.checked_add(1)
                    != Some(member.metadata.identity.generation)
            })
            || !checkpoints.insert(member.metadata.checkpoint_id)
            || !adoption_effects.insert(member.metadata.adoption_effect_id)
            || previous.is_some_and(|prior| {
                member.metadata.capture_generation < prior.metadata.capture_generation
                    || (member.metadata.capture_generation == prior.metadata.capture_generation
                        && (member.metadata.adoption_mux_incarnation
                            != prior.metadata.adoption_mux_incarnation
                            || member.metadata.adoption_sequence
                                <= prior.metadata.adoption_sequence))
            })
        {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        if let Some(existing_checkpoint) =
            boundaries.insert(member.metadata.boundary_id, member.metadata.checkpoint_id)
            && existing_checkpoint != member.metadata.checkpoint_id
        {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        previous = Some(member);
    }
    Ok(())
}

fn checkpoint_catalog_scan(
    inner: &GuardianCheckpointStageStoreInner,
    scope: CheckpointCatalogScope,
) -> Result<CheckpointCatalogScan, GuardianCheckpointStageStoreError> {
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    let mut candidates = Vec::new();
    let mut markers = Vec::new();
    let mut staged_files = Vec::new();
    candidates
        .try_reserve_exact(CHECKPOINT_CATALOG_MAX_RELEVANT_FILES)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    markers
        .try_reserve_exact(CHECKPOINT_CATALOG_MAX_RELEVANT_FILES)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    staged_files
        .try_reserve_exact(CHECKPOINT_CATALOG_MAX_RELEVANT_FILES)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    let mut relevant_files = 0_usize;
    let mut relevant_bytes = 0_u64;
    for file_name in read_directory_names(&inner.directory)? {
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with(CHECKPOINT_CATALOG_FILE_PREFIX) {
            continue;
        }
        let (identity, kind) = checkpoint_catalog_parse_path(name)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        if identity.scope != scope {
            continue;
        }
        relevant_files = relevant_files
            .checked_add(1)
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        if relevant_files > CHECKPOINT_CATALOG_MAX_RELEVANT_FILES {
            return Err(GuardianCheckpointStageStoreError::Capacity);
        }
        let path = inner.directory_path.join(&file_name);
        let file = open_private_file_at(&inner.directory, &inner.directory_path, &path, false)?;
        let metadata = file.metadata().map_err(|error| {
            GuardianCheckpointStageStoreError::io("checkpoint-catalog-census-metadata", error)
        })?;
        validate_private_file_metadata(&metadata, None)?;
        let file_identity = FileIdentity::capture(&metadata, Some(metadata.len()));
        validate_file_identity_at(
            &inner.directory,
            &inner.directory_path,
            &path,
            file_identity,
        )?;
        relevant_bytes = relevant_bytes
            .checked_add(metadata.len())
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        if relevant_bytes > CHECKPOINT_CATALOG_MAX_RELEVANT_BYTES {
            return Err(GuardianCheckpointStageStoreError::Capacity);
        }
        match kind {
            CheckpointCatalogPathKind::Canonical(CheckpointCatalogPathRole::Candidate) => {
                if metadata.len() == 0 || metadata.len() > CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                candidates.push(DiscoveredCheckpointCatalogCandidate {
                    identity,
                    path,
                    file_identity,
                    bytes: metadata.len(),
                    published: false,
                });
            }
            CheckpointCatalogPathKind::Canonical(CheckpointCatalogPathRole::Marker) => {
                if metadata.len()
                    != u64::try_from(CHECKPOINT_CATALOG_MARKER_BYTES)
                        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?
                {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                let bytes = checkpoint_catalog_read_file(
                    inner,
                    &path,
                    file_identity,
                    u64::try_from(CHECKPOINT_CATALOG_MARKER_BYTES)
                        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
                )?;
                let marker = checkpoint_catalog_decode_marker(&bytes)?;
                if marker.identity != identity {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                markers.push(DiscoveredCheckpointCatalogMarker {
                    marker,
                    path,
                    file_identity,
                });
            }
            CheckpointCatalogPathKind::Staging(role) => {
                let maximum = match role {
                    CheckpointCatalogPathRole::Candidate => CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES,
                    CheckpointCatalogPathRole::Marker => {
                        u64::try_from(CHECKPOINT_CATALOG_MARKER_BYTES)
                            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?
                    }
                };
                if metadata.len() > maximum {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                staged_files.push(DiscoveredCheckpointCatalogStagingFile {
                    identity,
                    role,
                    path,
                    file_identity,
                    bytes: metadata.len(),
                });
            }
        }
    }

    if markers.len() > CHECKPOINT_CATALOG_MAX_PUBLISHED_MEMBERS {
        return Err(GuardianCheckpointStageStoreError::Capacity);
    }
    let mut published = Vec::new();
    published
        .try_reserve_exact(markers.len())
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    for discovered_marker in markers {
        let candidate_entry = candidates
            .iter_mut()
            .find(|candidate| candidate.identity == discovered_marker.marker.identity)
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        if candidate_entry.published {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        let candidate_bytes = checkpoint_catalog_read_file(
            inner,
            &candidate_entry.path,
            candidate_entry.file_identity,
            CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES,
        )?;
        if u64::try_from(candidate_bytes.len()).ok() != Some(candidate_entry.bytes) {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        let candidate = checkpoint_catalog_decode_candidate(&candidate_bytes)?;
        if candidate.metadata.identity != candidate_entry.identity
            || !checkpoint_catalog_marker_matches_candidate(&discovered_marker.marker, &candidate)
        {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        checkpoint_catalog_validate_candidate_records(inner, &candidate)?;
        match (candidate.format, candidate.metadata.identity.scope) {
            (CheckpointCatalogFormat::ProtectedV3, CheckpointCatalogScope::Pane { .. }) => {
                drop(checkpoint_catalog_recover_adoption_evidence(
                    inner, &candidate,
                )?);
            }
            (
                CheckpointCatalogFormat::LegacyV2,
                CheckpointCatalogScope::Pane { .. } | CheckpointCatalogScope::Genesis { .. },
            )
            | (CheckpointCatalogFormat::ProtectedV3, CheckpointCatalogScope::Genesis { .. }) => {
                if candidate.adoption_evidence.is_some() {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
            }
        }
        candidate_entry.published = true;
        published.push(PublishedCheckpointCatalogMember {
            format: candidate.format,
            metadata: candidate.metadata,
            candidate_checksum: candidate.checksum,
            candidate_path: candidate_entry.path.clone(),
            candidate_file_identity: candidate_entry.file_identity,
            marker_path: discovered_marker.path,
            marker_file_identity: discovered_marker.file_identity,
        });
    }
    checkpoint_catalog_validate_chain(&mut published)?;
    let unpublished_candidates = candidates
        .into_iter()
        .filter(|candidate| !candidate.published)
        .collect();
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    Ok(CheckpointCatalogScan {
        published,
        unpublished_candidates,
        staged_files,
        relevant_files,
        relevant_bytes,
    })
}

fn checkpoint_catalog_stage_records(
    inner: &GuardianCheckpointStageStoreInner,
    census: &CheckpointStageCensus,
    shape: &CheckpointStageRequestShape,
    inspection: &CheckpointStageUploadInspection,
) -> Result<Vec<GuardianEncryptedCheckpointStageRecordV1>, GuardianCheckpointStageStoreError> {
    let complete_geometry = inspection.next_index == shape.total_chunks
        && inspection.committed_bytes == shape.total_bytes;
    if !complete_geometry || !inspection.seal_present || inspection.expiry_present {
        return Err(GuardianCheckpointStageStoreError::OutOfOrder);
    }
    let record_count = usize::try_from(shape.total_chunks)
        .ok()
        .and_then(|chunks| chunks.checked_add(2))
        .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(record_count)
        .map_err(|_| GuardianCheckpointStageStoreError::Allocation)?;
    let candidate_entry = census
        .entries
        .iter()
        .find(|entry| entry.key == shape.key() && entry.role == CheckpointStageFileRole::Candidate)
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    let (_, candidate_record, _) = checkpoint_read_record(
        inner,
        candidate_entry,
        u32::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES)
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
    )?;
    records.push(candidate_record);
    for index in 0..shape.total_chunks {
        let entry = census
            .entries
            .iter()
            .find(|entry| {
                entry.key == shape.key()
                    && entry.role
                        == CheckpointStageFileRole::Chunk {
                            publication_id: inspection.publication_id,
                            index,
                        }
            })
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let remaining = shape
            .total_bytes
            .checked_sub(
                u64::from(index)
                    .checked_mul(u64::from(shape.chunk_bytes))
                    .ok_or(GuardianCheckpointStageStoreError::Capacity)?,
            )
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        let max_plaintext_bytes = u32::try_from(remaining.min(u64::from(shape.chunk_bytes)))
            .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
        let (_, record, _) = checkpoint_read_record(inner, entry, max_plaintext_bytes)?;
        records.push(record);
    }
    let seal_entry = census
        .entries
        .iter()
        .find(|entry| {
            entry.key == shape.key()
                && entry.role
                    == CheckpointStageFileRole::Seal {
                        publication_id: inspection.publication_id,
                    }
        })
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    let (_, seal_record, _) =
        checkpoint_read_record(inner, seal_entry, GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)?;
    records.push(seal_record);
    if records.len() != record_count {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    Ok(records)
}

fn checkpoint_catalog_candidate_from_sealed_stage(
    inner: &GuardianCheckpointStageStoreInner,
    request: &GuardianCheckpointStageRequestV1,
    identity: CheckpointCatalogIdentity,
    predecessor: Option<CheckpointCatalogPredecessor>,
    seed: &GuardianCheckpointCatalogAdoptionEvidenceSeedV1,
) -> Result<CheckpointCatalogCandidate, GuardianCheckpointStageStoreError> {
    if request.kind() != GuardianCheckpointStageKindV1::Seal {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let shape = CheckpointStageRequestShape::from_request(request)?;
    if !matches!(identity.scope, CheckpointCatalogScope::Pane { .. })
        || CheckpointCatalogScope::from_stage_scope(shape.path_scope) != identity.scope
        || seed.pane_id() != identity.scope.identity()
        || seed.generation() != shape.descriptor.capture_generation()
        || seed.mux_incarnation().is_nil()
        || seed.canonical_request_id().is_nil()
        || seed.sequence() == 0
        || seed.effect_id().is_nil()
        || seed.checkpoint_identity_digest() != shape.descriptor.checkpoint_id().into_bytes()
        || seed.output_boundary_identity_digest() != shape.descriptor.boundary_id().into_bytes()
    {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let census = checkpoint_stage_census(inner)?;
    let inspection = checkpoint_inspect_upload(
        inner,
        &census,
        &shape,
        CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
    )?
    .ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
    let payload = checkpoint_assemble_payload(inner, &census, &shape, inspection.publication_id)?;
    shape.descriptor.validate_canonical_payload(&payload)?;
    let records = checkpoint_catalog_stage_records(inner, &census, &shape, &inspection)?;
    let candidate = CheckpointCatalogCandidate {
        format: CheckpointCatalogFormat::ProtectedV3,
        metadata: CheckpointCatalogMetadata {
            identity,
            predecessor,
            upload_id: shape.upload_id,
            completion_id: inspection.publication_id,
            checkpoint_id: shape.descriptor.checkpoint_id().into_bytes(),
            boundary_id: shape.descriptor.boundary_id().into_bytes(),
            terminal_payload_digest: shape.descriptor.terminal_payload_digest(),
            total_bytes: shape.total_bytes,
            chunk_count: shape.total_chunks,
            capture_generation: shape.descriptor.capture_generation(),
            replay_semantics_id: shape.descriptor.replay_semantics_id(),
            rows: shape.descriptor.rows(),
            cols: shape.descriptor.cols(),
            adoption_mux_incarnation: seed.mux_incarnation(),
            adoption_effect_id: seed.effect_id(),
            adoption_sequence: seed.sequence(),
            genesis_durable_pane_id: Uuid::nil(),
            genesis_origin_request_id: Uuid::nil(),
            genesis_spawn_payload_bytes: 0,
            genesis_spawn_payload_digest: [0; 32],
            genesis_spawning_mux_build_identity_digest: [0; 32],
            genesis_live_guardian_build_identity_digest: [0; 32],
            genesis_pixel_width: 0,
            genesis_pixel_height: 0,
        },
        records,
        checksum: [0; OUTPUT_MANIFEST_CHECKSUM_BYTES],
        adoption_evidence: None,
    };
    checkpoint_catalog_validate_candidate_records(inner, &candidate)?;
    Ok(candidate)
}

fn checkpoint_catalog_validate_genesis_reservation(
    reservation: CheckpointCatalogGenesisReservationBinding,
) -> Result<(), GuardianCheckpointStageStoreError> {
    if reservation.mux_incarnation.is_nil()
        || reservation.spawn_effect_id.is_nil()
        || reservation.durable_pane_id.is_nil()
        || reservation.origin_request_id.is_nil()
        || reservation.spawn_payload_bytes == 0
        || reservation.spawn_payload_digest == [0; 32]
        || reservation.spawning_mux_build_identity_digest == [0; 32]
        || reservation.live_guardian_build_identity_digest == [0; 32]
        || reservation.rows == 0
        || reservation.cols == 0
        || reservation.checkpoint_identity_digest == [0; 32]
        || reservation.boundary_identity_digest == [0; 32]
        || reservation.upload_id.is_nil()
    {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    Ok(())
}

fn checkpoint_catalog_candidate_from_genesis_stage(
    inner: &GuardianCheckpointStageStoreInner,
    request: &GuardianCheckpointStageRequestV1,
    identity: CheckpointCatalogIdentity,
    reservation: CheckpointCatalogGenesisReservationBinding,
) -> Result<CheckpointCatalogCandidate, GuardianCheckpointStageStoreError> {
    checkpoint_catalog_validate_genesis_reservation(reservation)?;
    if request.kind() != GuardianCheckpointStageKindV1::Seal {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let shape = CheckpointStageRequestShape::from_request(request)?;
    let expected_scope = CheckpointCatalogScope::Genesis {
        spawn_effect_id: reservation.spawn_effect_id,
    };
    if identity.scope != expected_scope
        || identity.generation != 1
        || identity.candidate_id != checkpoint_catalog_genesis_candidate_id(reservation)
        || CheckpointCatalogScope::from_stage_scope(shape.path_scope) != expected_scope
        || shape.upload_id != reservation.upload_id
        || shape.descriptor.capture_generation() != 1
        || shape.descriptor.checkpoint_id().into_bytes() != reservation.checkpoint_identity_digest
        || shape.descriptor.boundary_id().into_bytes() != reservation.boundary_identity_digest
        || shape.descriptor.rows() != u32::from(reservation.rows)
        || shape.descriptor.cols() != u32::from(reservation.cols)
    {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let census = checkpoint_stage_census(inner)?;
    let inspection = checkpoint_inspect_upload(
        inner,
        &census,
        &shape,
        CheckpointStageSealInspection::IgnoreForHistoricalChunkRetry,
    )?
    .ok_or(GuardianCheckpointStageStoreError::CandidateAbsent)?;
    let payload = checkpoint_assemble_payload(inner, &census, &shape, inspection.publication_id)?;
    shape.descriptor.validate_canonical_payload(&payload)?;
    let records = checkpoint_catalog_stage_records(inner, &census, &shape, &inspection)?;
    let candidate = CheckpointCatalogCandidate {
        format: CheckpointCatalogFormat::ProtectedV3,
        metadata: CheckpointCatalogMetadata {
            identity,
            predecessor: None,
            upload_id: shape.upload_id,
            completion_id: inspection.publication_id,
            checkpoint_id: shape.descriptor.checkpoint_id().into_bytes(),
            boundary_id: shape.descriptor.boundary_id().into_bytes(),
            terminal_payload_digest: shape.descriptor.terminal_payload_digest(),
            total_bytes: shape.total_bytes,
            chunk_count: shape.total_chunks,
            capture_generation: shape.descriptor.capture_generation(),
            replay_semantics_id: shape.descriptor.replay_semantics_id(),
            rows: shape.descriptor.rows(),
            cols: shape.descriptor.cols(),
            adoption_mux_incarnation: reservation.mux_incarnation,
            adoption_effect_id: reservation.spawn_effect_id,
            adoption_sequence: 0,
            genesis_durable_pane_id: reservation.durable_pane_id,
            genesis_origin_request_id: reservation.origin_request_id,
            genesis_spawn_payload_bytes: reservation.spawn_payload_bytes,
            genesis_spawn_payload_digest: reservation.spawn_payload_digest,
            genesis_spawning_mux_build_identity_digest: reservation
                .spawning_mux_build_identity_digest,
            genesis_live_guardian_build_identity_digest: reservation
                .live_guardian_build_identity_digest,
            genesis_pixel_width: reservation.pixel_width,
            genesis_pixel_height: reservation.pixel_height,
        },
        records,
        checksum: [0; OUTPUT_MANIFEST_CHECKSUM_BYTES],
        adoption_evidence: None,
    };
    checkpoint_catalog_validate_candidate_records(inner, &candidate)?;
    Ok(candidate)
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn checkpoint_catalog_publish_noreplace(
    directory: &File,
    staging_name: &OsStr,
    canonical_name: &OsStr,
) -> std::io::Result<()> {
    rustix::fs::renameat_with(
        directory,
        staging_name,
        directory,
        canonical_name,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn checkpoint_catalog_publish_noreplace(
    _directory: &File,
    _staging_name: &OsStr,
    _canonical_name: &OsStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "atomic no-replace checkpoint catalog publication is unsupported on this Unix target",
    ))
}

fn checkpoint_catalog_verify_file_prefix(
    file: &mut File,
    expected: &[u8],
    observed_len: u64,
    read_site: &'static str,
) -> Result<usize, GuardianCheckpointStageStoreError> {
    let observed_len =
        usize::try_from(observed_len).map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
    if observed_len > expected.len() {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| GuardianCheckpointStageStoreError::io(read_site, error))?;
    let mut offset = 0_usize;
    let mut buffer = [0_u8; 8 * 1024];
    while offset < observed_len {
        let chunk = buffer.len().min(observed_len - offset);
        file.read_exact(&mut buffer[..chunk])
            .map_err(|error| GuardianCheckpointStageStoreError::io(read_site, error))?;
        if !checkpoint_bytes_match(&buffer[..chunk], &expected[offset..offset + chunk]) {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        offset = offset
            .checked_add(chunk)
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    }
    Ok(observed_len)
}

fn checkpoint_catalog_publish_file(
    inner: &GuardianCheckpointStageStoreInner,
    path: &Path,
    bytes: &[u8],
    write_site: &'static str,
    sync_site: &'static str,
) -> Result<FileIdentity, GuardianCheckpointStageStoreError> {
    let expected_len =
        u64::try_from(bytes.len()).map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
    match open_private_file_at(&inner.directory, &inner.directory_path, path, false) {
        Ok(mut existing) => {
            let before = existing.metadata().map_err(|error| {
                GuardianCheckpointStageStoreError::io("checkpoint-catalog-existing-metadata", error)
            })?;
            validate_private_file_metadata(&before, Some(expected_len))?;
            checkpoint_catalog_verify_file_prefix(
                &mut existing,
                bytes,
                expected_len,
                "checkpoint-catalog-existing-read",
            )?;
            let identity = FileIdentity::capture(&before, Some(expected_len));
            existing
                .sync_all()
                .map_err(|error| GuardianCheckpointStageStoreError::io(sync_site, error))?;
            inner.directory.sync_all().map_err(|error| {
                GuardianCheckpointStageStoreError::io(
                    "checkpoint-catalog-existing-directory-sync",
                    error,
                )
            })?;
            inner
                .persistence
                .validate(&inner.directory)
                .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
            validate_file_identity_at(&inner.directory, &inner.directory_path, path, identity)?;
            return Ok(identity);
        }
        Err(GuardianOutputError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let staging_path = checkpoint_catalog_staging_path(inner, path)?;
    let mut file =
        match create_private_file_new_at(&inner.directory, &inner.directory_path, &staging_path) {
            Ok(file) => file,
            Err(GuardianOutputError::Io { source, .. })
                if source.kind() == ErrorKind::AlreadyExists =>
            {
                open_private_file_at(
                    &inner.directory,
                    &inner.directory_path,
                    &staging_path,
                    false,
                )?
            }
            Err(error) => return Err(error.into()),
        };
    let before = file.metadata().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-catalog-staging-metadata", error)
    })?;
    validate_private_file_metadata(&before, None)?;
    if before.len() > expected_len {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let staging_identity = FileIdentity::capture(&before, None);
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &staging_path,
        FileIdentity::capture(&before, Some(before.len())),
    )?;
    let observed_len = checkpoint_catalog_verify_file_prefix(
        &mut file,
        bytes,
        before.len(),
        "checkpoint-catalog-staging-prefix-read",
    )?;
    let after_prefix = file.metadata().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-catalog-staging-prefix-metadata", error)
    })?;
    if !staging_identity.matches(&after_prefix) || after_prefix.len() != before.len() {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    file.seek(SeekFrom::Start(before.len()))
        .map_err(|error| GuardianCheckpointStageStoreError::io(write_site, error))?;
    file.write_all(&bytes[observed_len..])
        .map_err(|error| GuardianCheckpointStageStoreError::io(write_site, error))?;
    file.sync_all()
        .map_err(|error| GuardianCheckpointStageStoreError::io(sync_site, error))?;
    let metadata = file.metadata().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-catalog-created-metadata", error)
    })?;
    validate_private_file_metadata(&metadata, Some(expected_len))?;
    let identity = FileIdentity::capture(&metadata, Some(expected_len));
    if !staging_identity.matches(&metadata) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    checkpoint_catalog_verify_file_prefix(
        &mut file,
        bytes,
        expected_len,
        "checkpoint-catalog-staging-complete-read",
    )?;
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &staging_path,
        identity,
    )?;
    let staging_name = output_child_name(&inner.directory_path, &staging_path)?;
    let canonical_name = output_child_name(&inner.directory_path, path)?;
    checkpoint_catalog_publish_noreplace(&inner.directory, staging_name, canonical_name).map_err(
        |error| {
            GuardianCheckpointStageStoreError::io("checkpoint-catalog-atomic-publication", error)
        },
    )?;
    inner.directory.sync_all().map_err(|error| {
        GuardianCheckpointStageStoreError::io(
            "checkpoint-catalog-publication-directory-sync",
            error,
        )
    })?;
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    validate_file_identity_at(&inner.directory, &inner.directory_path, path, identity)?;
    let mut published = open_private_file_at(&inner.directory, &inner.directory_path, path, false)?;
    checkpoint_catalog_verify_file_prefix(
        &mut published,
        bytes,
        expected_len,
        "checkpoint-catalog-published-read",
    )?;
    Ok(identity)
}

fn checkpoint_catalog_resync_file(
    inner: &GuardianCheckpointStageStoreInner,
    path: &Path,
    identity: FileIdentity,
    sync_site: &'static str,
) -> Result<(), GuardianCheckpointStageStoreError> {
    let file = open_private_file_at(&inner.directory, &inner.directory_path, path, false)?;
    let metadata = file.metadata().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-catalog-resync-metadata", error)
    })?;
    validate_private_file_metadata(&metadata, identity.expected_len)?;
    if !identity.matches(&metadata) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    file.sync_all()
        .map_err(|error| GuardianCheckpointStageStoreError::io(sync_site, error))?;
    inner.directory.sync_all().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-catalog-resync-directory", error)
    })?;
    inner
        .persistence
        .validate(&inner.directory)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
    validate_file_identity_at(&inner.directory, &inner.directory_path, path, identity)?;
    Ok(())
}

enum CheckpointCatalogGenesisCandidatePlan<'a> {
    Create,
    Reuse(&'a DiscoveredCheckpointCatalogCandidate),
}

fn checkpoint_catalog_staging_role(
    inner: &GuardianCheckpointStageStoreInner,
    scan: &CheckpointCatalogScan,
    identity: CheckpointCatalogIdentity,
) -> Result<Option<CheckpointCatalogPathRole>, GuardianCheckpointStageStoreError> {
    let staged = match scan.staged_files.as_slice() {
        [] => return Ok(None),
        [staged] => staged,
        _ => return Err(GuardianCheckpointStageStoreError::Poisoned),
    };
    if staged.identity != identity {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let canonical = checkpoint_catalog_path(inner, identity, staged.role)?;
    let expected_staging = checkpoint_catalog_staging_path(inner, &canonical)?;
    if staged.path != expected_staging || staged.file_identity.expected_len != Some(staged.bytes) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &staged.path,
        staged.file_identity,
    )?;
    Ok(Some(staged.role))
}

fn checkpoint_catalog_genesis_candidate_plan<'a>(
    scan: &'a CheckpointCatalogScan,
    identity: CheckpointCatalogIdentity,
    candidate_path: &Path,
) -> Result<CheckpointCatalogGenesisCandidatePlan<'a>, GuardianCheckpointStageStoreError> {
    match scan.unpublished_candidates.as_slice() {
        [] => Ok(CheckpointCatalogGenesisCandidatePlan::Create),
        [existing] if existing.identity == identity && existing.path == candidate_path => {
            Ok(CheckpointCatalogGenesisCandidatePlan::Reuse(existing))
        }
        _ => Err(GuardianCheckpointStageStoreError::Poisoned),
    }
}

fn checkpoint_catalog_genesis_added_resources(
    plan: &CheckpointCatalogGenesisCandidatePlan<'_>,
    staged: Option<&DiscoveredCheckpointCatalogStagingFile>,
    candidate_bytes: usize,
    marker_bytes: usize,
) -> Result<(usize, u64), GuardianCheckpointStageStoreError> {
    let (mut added_files, mut added_bytes) = match plan {
        CheckpointCatalogGenesisCandidatePlan::Create => {
            let bytes = candidate_bytes
                .checked_add(marker_bytes)
                .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
            (2_usize, bytes)
        }
        CheckpointCatalogGenesisCandidatePlan::Reuse(_) => (1_usize, marker_bytes),
    };
    if let Some(staged) = staged {
        let expected_role = match plan {
            CheckpointCatalogGenesisCandidatePlan::Create => CheckpointCatalogPathRole::Candidate,
            CheckpointCatalogGenesisCandidatePlan::Reuse(_) => CheckpointCatalogPathRole::Marker,
        };
        let expected_bytes = match expected_role {
            CheckpointCatalogPathRole::Candidate => candidate_bytes,
            CheckpointCatalogPathRole::Marker => marker_bytes,
        };
        if staged.role != expected_role
            || usize::try_from(staged.bytes)
                .ok()
                .is_none_or(|bytes| bytes > expected_bytes)
        {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        added_files = added_files
            .checked_sub(1)
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
        added_bytes = added_bytes
            .checked_sub(
                usize::try_from(staged.bytes)
                    .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
            )
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?;
    }
    Ok((
        added_files,
        u64::try_from(added_bytes).map_err(|_| GuardianCheckpointStageStoreError::Capacity)?,
    ))
}

fn checkpoint_catalog_require_exact_genesis_candidate_bytes(
    observed: &[u8],
    expected: &[u8],
) -> Result<(), GuardianCheckpointStageStoreError> {
    if checkpoint_bytes_match(observed, expected) {
        Ok(())
    } else {
        Err(GuardianCheckpointStageStoreError::Poisoned)
    }
}

fn checkpoint_catalog_publish_genesis_stage(
    inner: &GuardianCheckpointStageStoreInner,
    request: &GuardianCheckpointStageRequestV1,
    reservation: CheckpointCatalogGenesisReservationBinding,
) -> Result<PublishedCheckpointCatalogMember, GuardianCheckpointStageStoreError> {
    checkpoint_catalog_validate_genesis_reservation(reservation)?;
    if request.kind() != GuardianCheckpointStageKindV1::Seal {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let shape = CheckpointStageRequestShape::from_request(request)?;
    let scope = CheckpointCatalogScope::Genesis {
        spawn_effect_id: reservation.spawn_effect_id,
    };
    if CheckpointCatalogScope::from_stage_scope(shape.path_scope) != scope
        || shape.upload_id != reservation.upload_id
    {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let identity = CheckpointCatalogIdentity {
        scope,
        generation: 1,
        candidate_id: checkpoint_catalog_genesis_candidate_id(reservation),
    };
    let scan = checkpoint_catalog_scan(inner, scope)?;
    if let Some(existing) = scan.published.first() {
        if scan.published.len() != 1
            || !scan.unpublished_candidates.is_empty()
            || !scan.staged_files.is_empty()
            || !checkpoint_catalog_genesis_metadata_matches_reservation(
                &existing.metadata,
                reservation,
            )
        {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        checkpoint_catalog_resync_file(
            inner,
            &existing.candidate_path,
            existing.candidate_file_identity,
            "checkpoint-catalog-genesis-existing-candidate-sync",
        )?;
        checkpoint_catalog_resync_file(
            inner,
            &existing.marker_path,
            existing.marker_file_identity,
            "checkpoint-catalog-genesis-existing-marker-sync",
        )?;
        let recovered = checkpoint_catalog_scan(inner, scope)?;
        let recovered_member = recovered
            .published
            .first()
            .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
        if recovered.published.len() != 1
            || !recovered.unpublished_candidates.is_empty()
            || !recovered.staged_files.is_empty()
            || recovered_member.metadata != existing.metadata
            || recovered_member.candidate_checksum != existing.candidate_checksum
            || recovered_member.candidate_file_identity != existing.candidate_file_identity
            || recovered_member.marker_file_identity != existing.marker_file_identity
        {
            return Err(GuardianCheckpointStageStoreError::Poisoned);
        }
        return Ok(recovered_member.clone());
    }
    let mut candidate =
        checkpoint_catalog_candidate_from_genesis_stage(inner, request, identity, reservation)?;
    let encoded_candidate = checkpoint_catalog_encode_candidate(&mut candidate)?;
    let marker = checkpoint_catalog_marker_for_candidate(&candidate);
    let encoded_marker = checkpoint_catalog_encode_marker(&marker);
    let candidate_path =
        checkpoint_catalog_path(inner, identity, CheckpointCatalogPathRole::Candidate)?;
    let candidate_plan =
        checkpoint_catalog_genesis_candidate_plan(&scan, identity, &candidate_path)?;
    let staging_role = checkpoint_catalog_staging_role(inner, &scan, identity)?;
    if !matches!(
        (&candidate_plan, staging_role),
        (
            CheckpointCatalogGenesisCandidatePlan::Create,
            None | Some(CheckpointCatalogPathRole::Candidate)
        ) | (
            CheckpointCatalogGenesisCandidatePlan::Reuse(_),
            None | Some(CheckpointCatalogPathRole::Marker)
        )
    ) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let (added_files, added_bytes) = checkpoint_catalog_genesis_added_resources(
        &candidate_plan,
        scan.staged_files.first(),
        encoded_candidate.len(),
        encoded_marker.len(),
    )?;
    if scan
        .relevant_files
        .checked_add(added_files)
        .is_none_or(|files| files > CHECKPOINT_CATALOG_MAX_RELEVANT_FILES)
        || scan
            .relevant_bytes
            .checked_add(added_bytes)
            .is_none_or(|bytes| bytes > CHECKPOINT_CATALOG_MAX_RELEVANT_BYTES)
    {
        return Err(GuardianCheckpointStageStoreError::Capacity);
    }
    let candidate_file_identity = match candidate_plan {
        CheckpointCatalogGenesisCandidatePlan::Reuse(existing) => {
            let existing_bytes = checkpoint_catalog_read_file(
                inner,
                &existing.path,
                existing.file_identity,
                CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES,
            )?;
            checkpoint_catalog_require_exact_genesis_candidate_bytes(
                &existing_bytes,
                &encoded_candidate,
            )?;
            let decoded = checkpoint_catalog_decode_candidate(&existing_bytes)?;
            if decoded.metadata.identity != identity
                || decoded.metadata != candidate.metadata
                || decoded.checksum != candidate.checksum
            {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            checkpoint_catalog_validate_candidate_records(inner, &decoded)?;
            checkpoint_catalog_resync_file(
                inner,
                &existing.path,
                existing.file_identity,
                "checkpoint-catalog-genesis-candidate-retry-sync",
            )?;
            existing.file_identity
        }
        CheckpointCatalogGenesisCandidatePlan::Create => checkpoint_catalog_publish_file(
            inner,
            &candidate_path,
            &encoded_candidate,
            "checkpoint-catalog-genesis-candidate-write",
            "checkpoint-catalog-genesis-candidate-sync",
        )?,
    };
    inner.directory.sync_all().map_err(|error| {
        GuardianCheckpointStageStoreError::io(
            "checkpoint-catalog-genesis-candidate-directory-sync",
            error,
        )
    })?;
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &candidate_path,
        candidate_file_identity,
    )?;

    let marker_path = checkpoint_catalog_path(inner, identity, CheckpointCatalogPathRole::Marker)?;
    let marker_file_identity = checkpoint_catalog_publish_file(
        inner,
        &marker_path,
        &encoded_marker,
        "checkpoint-catalog-genesis-marker-write",
        "checkpoint-catalog-genesis-marker-sync",
    )?;
    inner.directory.sync_all().map_err(|error| {
        GuardianCheckpointStageStoreError::io(
            "checkpoint-catalog-genesis-marker-directory-sync",
            error,
        )
    })?;
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &candidate_path,
        candidate_file_identity,
    )?;
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &marker_path,
        marker_file_identity,
    )?;

    let recovered = checkpoint_catalog_scan(inner, scope)?;
    let head = recovered
        .published
        .first()
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    if recovered.published.len() != 1
        || !recovered.unpublished_candidates.is_empty()
        || !recovered.staged_files.is_empty()
        || head.metadata.identity != identity
        || !checkpoint_catalog_genesis_metadata_matches_reservation(&head.metadata, reservation)
        || head.candidate_checksum != candidate.checksum
        || head.candidate_path != candidate_path
        || head.candidate_file_identity != candidate_file_identity
        || head.marker_path != marker_path
        || head.marker_file_identity != marker_file_identity
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    Ok(head.clone())
}

fn checkpoint_catalog_publish_sealed_stage(
    inner: &GuardianCheckpointStageStoreInner,
    request: &GuardianCheckpointStageRequestV1,
    seed: GuardianCheckpointCatalogAdoptionEvidenceSeedV1,
) -> Result<PublishedCheckpointCatalogMember, GuardianCheckpointStageStoreError> {
    if request.kind() != GuardianCheckpointStageKindV1::Seal {
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    let shape = CheckpointStageRequestShape::from_request(request)?;
    let scope = CheckpointCatalogScope::from_stage_scope(shape.path_scope);
    let scan = checkpoint_catalog_scan(inner, scope)?;
    if let Some(existing) = scan.published.iter().find(|member| {
        member.metadata.checkpoint_id == shape.descriptor.checkpoint_id().into_bytes()
            || member.metadata.boundary_id == shape.descriptor.boundary_id().into_bytes()
    }) {
        if existing.metadata.checkpoint_id == shape.descriptor.checkpoint_id().into_bytes()
            && existing.metadata.boundary_id == shape.descriptor.boundary_id().into_bytes()
            && existing.metadata.upload_id == shape.upload_id
            && existing.metadata.adoption_mux_incarnation == seed.mux_incarnation()
            && existing.metadata.adoption_effect_id == seed.effect_id()
            && existing.metadata.adoption_sequence == seed.sequence()
            && existing.format == CheckpointCatalogFormat::ProtectedV3
        {
            if !scan.unpublished_candidates.is_empty() || !scan.staged_files.is_empty() {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            let existing_bytes = checkpoint_catalog_read_file(
                inner,
                &existing.candidate_path,
                existing.candidate_file_identity,
                CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES,
            )?;
            let recovered_candidate = checkpoint_catalog_decode_candidate(&existing_bytes)?;
            if recovered_candidate.metadata != existing.metadata
                || recovered_candidate.checksum != existing.candidate_checksum
            {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            checkpoint_catalog_validate_candidate_records(inner, &recovered_candidate)?;
            drop(checkpoint_catalog_recover_adoption_evidence_with_seed(
                inner,
                &recovered_candidate,
                seed,
            )?);
            checkpoint_catalog_resync_file(
                inner,
                &existing.candidate_path,
                existing.candidate_file_identity,
                "checkpoint-catalog-existing-candidate-sync",
            )?;
            checkpoint_catalog_resync_file(
                inner,
                &existing.marker_path,
                existing.marker_file_identity,
                "checkpoint-catalog-existing-marker-sync",
            )?;
            return Ok(existing.clone());
        }
        return Err(GuardianCheckpointStageStoreError::Conflict);
    }
    if scan.published.len() >= CHECKPOINT_CATALOG_MAX_PUBLISHED_MEMBERS
        || scan
            .relevant_files
            .checked_add(2)
            .is_none_or(|files| files > CHECKPOINT_CATALOG_MAX_RELEVANT_FILES)
    {
        return Err(GuardianCheckpointStageStoreError::Capacity);
    }
    let predecessor = scan
        .published
        .last()
        .map(|member| CheckpointCatalogPredecessor {
            generation: member.metadata.identity.generation,
            candidate_id: member.metadata.identity.candidate_id,
            candidate_checksum: member.candidate_checksum,
            checkpoint_id: member.metadata.checkpoint_id,
            boundary_id: member.metadata.boundary_id,
        });
    let generation = match predecessor {
        Some(previous) => previous
            .generation
            .checked_add(1)
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?,
        None => 1,
    };
    let identity = CheckpointCatalogIdentity {
        scope,
        generation,
        candidate_id: checkpoint_catalog_candidate_id(
            scope,
            generation,
            predecessor,
            &shape,
            &seed,
        ),
    };
    let candidate_path =
        checkpoint_catalog_path(inner, identity, CheckpointCatalogPathRole::Candidate)?;
    let candidate_plan =
        checkpoint_catalog_genesis_candidate_plan(&scan, identity, &candidate_path)?;
    let staging_role = checkpoint_catalog_staging_role(inner, &scan, identity)?;
    if !matches!(
        (&candidate_plan, staging_role),
        (
            CheckpointCatalogGenesisCandidatePlan::Create,
            None | Some(CheckpointCatalogPathRole::Candidate)
        ) | (
            CheckpointCatalogGenesisCandidatePlan::Reuse(_),
            None | Some(CheckpointCatalogPathRole::Marker)
        )
    ) {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let mut candidate = checkpoint_catalog_candidate_from_sealed_stage(
        inner,
        request,
        identity,
        predecessor,
        &seed,
    )?;
    let encoded_base = checkpoint_catalog_encode_candidate_base(&mut candidate)?;
    let (mut candidate, encoded_candidate) = match &candidate_plan {
        CheckpointCatalogGenesisCandidatePlan::Create => {
            candidate.adoption_evidence = Some(checkpoint_catalog_seal_adoption_evidence(
                inner, &candidate, seed,
            )?);
            let encoded_candidate = checkpoint_catalog_encode_candidate(&mut candidate)?;
            if encoded_candidate.get(..encoded_base.len()) != Some(encoded_base.as_slice()) {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            (candidate, encoded_candidate)
        }
        CheckpointCatalogGenesisCandidatePlan::Reuse(existing) => {
            let existing_bytes = checkpoint_catalog_read_file(
                inner,
                &existing.path,
                existing.file_identity,
                CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES,
            )?;
            let recovered_candidate = checkpoint_catalog_decode_candidate(&existing_bytes)?;
            if existing_bytes.get(..encoded_base.len()) != Some(encoded_base.as_slice())
                || recovered_candidate.metadata != candidate.metadata
                || recovered_candidate.checksum != candidate.checksum
            {
                return Err(GuardianCheckpointStageStoreError::Poisoned);
            }
            checkpoint_catalog_validate_candidate_records(inner, &recovered_candidate)?;
            drop(checkpoint_catalog_recover_adoption_evidence_with_seed(
                inner,
                &recovered_candidate,
                seed,
            )?);
            (recovered_candidate, existing_bytes)
        }
    };
    let marker = checkpoint_catalog_marker_for_candidate(&candidate);
    let encoded_marker = checkpoint_catalog_encode_marker(&marker);
    let (added_files, added_bytes) = checkpoint_catalog_genesis_added_resources(
        &candidate_plan,
        scan.staged_files.first(),
        encoded_candidate.len(),
        encoded_marker.len(),
    )?;
    if scan
        .relevant_files
        .checked_add(added_files)
        .is_none_or(|files| files > CHECKPOINT_CATALOG_MAX_RELEVANT_FILES)
        || scan
            .relevant_bytes
            .checked_add(added_bytes)
            .is_none_or(|bytes| bytes > CHECKPOINT_CATALOG_MAX_RELEVANT_BYTES)
    {
        return Err(GuardianCheckpointStageStoreError::Capacity);
    }
    let candidate_file_identity = match &candidate_plan {
        CheckpointCatalogGenesisCandidatePlan::Create => checkpoint_catalog_publish_file(
            inner,
            &candidate_path,
            &encoded_candidate,
            "checkpoint-catalog-candidate-write",
            "checkpoint-catalog-candidate-sync",
        )?,
        CheckpointCatalogGenesisCandidatePlan::Reuse(existing) => {
            checkpoint_catalog_resync_file(
                inner,
                &existing.path,
                existing.file_identity,
                "checkpoint-catalog-candidate-retry-sync",
            )?;
            existing.file_identity
        }
    };
    let marker_path = checkpoint_catalog_path(inner, identity, CheckpointCatalogPathRole::Marker)?;
    let marker_file_identity = checkpoint_catalog_publish_file(
        inner,
        &marker_path,
        &encoded_marker,
        "checkpoint-catalog-marker-write",
        "checkpoint-catalog-marker-sync",
    )?;
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &candidate_path,
        candidate_file_identity,
    )?;
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        &marker_path,
        marker_file_identity,
    )?;
    let recovered = checkpoint_catalog_scan(inner, scope)?;
    let head = recovered
        .published
        .last()
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    if recovered.published.len()
        != scan
            .published
            .len()
            .checked_add(1)
            .ok_or(GuardianCheckpointStageStoreError::Capacity)?
        || !recovered.unpublished_candidates.is_empty()
        || !recovered.staged_files.is_empty()
        || head.metadata.identity != identity
        || head.candidate_checksum != candidate.checksum
        || head.candidate_path != candidate_path
        || head.candidate_file_identity != candidate_file_identity
        || head.marker_path != marker_path
        || head.marker_file_identity != marker_file_identity
    {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    Ok(head.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_term::terminalstate::checkpoint::TerminalCheckpointLimits;
    use frankenterm_term::{
        RecoveryTerminalCheckpointV2, Terminal, TerminalConfiguration, TerminalSize,
    };
    use mio::{Poll, Token};
    use mux::guardian_protocol::{
        GuardianCheckpointIntent, GuardianEffectOutcome, GuardianOperation,
        GuardianRequestEnvelope, GuardianRequestHeader, GuardianSecret, GuardianSpawnPayload,
        decode_guardian_request, encode_guardian_request,
    };
    use portable_pty::{CommandBuilder, PtySize};
    use std::fs::hard_link;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::time::{Duration, Instant};

    fn zeroizing_test_bytes(bytes: &[u8]) -> Zeroizing<Vec<u8>> {
        let mut owned = Zeroizing::new(Vec::with_capacity(bytes.len()));
        owned.extend_from_slice(bytes);
        owned
    }

    #[test]
    fn directory_identity_ignores_child_link_count_but_rejects_mode_change()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-directory-identity-")?;
        let before = std::fs::symlink_metadata(&directory)?;
        let identity = DirectoryIdentity::capture(&before);

        std::fs::create_dir(directory.join("child"))?;
        let after_child = std::fs::symlink_metadata(&directory)?;
        assert_ne!(before.nlink(), after_child.nlink());
        assert!(identity.matches(&after_child));

        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o750))?;
        assert!(!identity.matches(&std::fs::symlink_metadata(&directory)?));
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        assert!(identity.matches(&std::fs::symlink_metadata(&directory)?));
        Ok(())
    }

    fn kept_private_directory(prefix: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let canonical_temp = crate::canonical_test_temp_root();
        let directory = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(canonical_temp)?
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        Ok(directory)
    }

    fn completion(
        pipeline: &GuardianOutputPipeline,
    ) -> Result<GuardianOutputCompletion, Box<dyn std::error::Error>> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match pipeline.try_completion() {
                GuardianOutputCompletionState::Ready(completion) => return Ok(completion),
                GuardianOutputCompletionState::Empty if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                GuardianOutputCompletionState::Empty => {
                    return Err("guardian output completion timed out".into());
                }
                GuardianOutputCompletionState::Disconnected => {
                    return Err("guardian output worker disconnected".into());
                }
            }
        }
    }

    fn tiny_rotation_policy(max_segments: usize) -> OutputSegmentPolicy {
        OutputSegmentPolicy {
            journal_limits: GuardianOutputJournalLimits {
                max_record_bytes: 64,
                max_log_bytes: 512,
                max_records: 1,
            },
            max_segments,
            max_durable_pane_bytes: 4 * 1024,
        }
    }

    fn pipeline_with_policy(
        prefix: &str,
        policy: OutputSegmentPolicy,
    ) -> Result<(PathBuf, Poll, GuardianOutputPipeline), Box<dyn std::error::Error>> {
        let directory = kept_private_directory(prefix)?;
        let token_path = directory.join("guardian.token");
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        let pipeline = GuardianOutputPipeline::open_with_policy(&token_path, 1, waker, policy)?;
        Ok((directory, poll, pipeline))
    }

    fn reopen_pipeline(
        directory: &Path,
        policy: OutputSegmentPolicy,
    ) -> Result<(Poll, GuardianOutputPipeline), Box<dyn std::error::Error>> {
        let token_path = directory.join("guardian.token");
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        let pipeline = GuardianOutputPipeline::open_with_policy(&token_path, 1, waker, policy)?;
        Ok((poll, pipeline))
    }

    fn durable_commit(
        pipeline: &GuardianOutputPipeline,
        pane_id: Uuid,
        journal: &GuardianPaneOutputJournal,
        payload: &[u8],
    ) -> Result<GuardianOutputAppendReceipt, Box<dyn std::error::Error>> {
        pipeline
            .try_submit(pane_id, journal.clone(), zeroizing_test_bytes(payload))
            .map_err(|_| "output submission was unexpectedly rejected")?;
        completion(pipeline)?
            .result
            .map_err(|_| "durable append failed".into())
    }

    fn checkpoint_test_terminal_digest(payload: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"frankenterm.guardian-checkpoint-terminal-payload.v1\0");
        hasher.update(
            u64::try_from(payload.len())
                .expect("fixture length fits u64")
                .to_le_bytes(),
        );
        hasher.update(payload);
        hasher.finalize().into()
    }

    #[derive(Debug)]
    struct CatalogCheckpointConfig;

    impl TerminalConfiguration for CatalogCheckpointConfig {
        fn color_palette(&self) -> frankenterm_term::color::ColorPalette {
            frankenterm_term::color::ColorPalette::default()
        }
    }

    fn checkpoint_catalog_test_terminal(content: &[u8]) -> RecoveryTerminalCheckpointV2 {
        let mut terminal = Terminal::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            Arc::new(CatalogCheckpointConfig),
            "FrankenTerm",
            "guardian-catalog-test",
            Box::new(Vec::<u8>::new()),
        );
        terminal.advance_bytes(content);
        terminal
            .capture_recovery_checkpoint(TerminalCheckpointLimits::default())
            .expect("capture canonical catalog checkpoint fixture")
    }

    #[allow(clippy::too_many_arguments)]
    fn checkpoint_catalog_authenticate_request(
        operation: GuardianOperation,
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
        request_id: Uuid,
        pane_id: Option<Uuid>,
        generation: u64,
        sequence: u64,
        effect_id: Option<Uuid>,
        payload: Zeroizing<Vec<u8>>,
    ) -> Result<AuthenticatedGuardianRequest, GuardianProtocolError> {
        let request = GuardianRequestEnvelope::from_zeroizing_payload(
            GuardianRequestHeader::new(
                operation,
                guardian_incarnation,
                mux_incarnation,
                request_id,
                pane_id,
                generation,
                sequence,
                effect_id,
                &payload,
            ),
            payload,
        );
        let secret = GuardianSecret::from_bytes([0x5a; 32])?;
        let frame = encode_guardian_request(&secret, &request)?;
        decode_guardian_request(&secret, &frame)
    }

    fn checkpoint_catalog_claimed_protocol_state(
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
        pane_id: Uuid,
    ) -> Result<GuardianProtocolState, Box<dyn std::error::Error>> {
        let mut state = GuardianProtocolState::new(guardian_incarnation)?;
        let spawn_payload = GuardianSpawnPayload::new(
            CommandBuilder::new("guardian-catalog-fixture"),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
            },
        )?
        .encode()?;
        let spawn = checkpoint_catalog_authenticate_request(
            GuardianOperation::Spawn,
            guardian_incarnation,
            mux_incarnation,
            Uuid::from_u128(0xca01),
            Some(pane_id),
            0,
            0,
            Some(Uuid::from_u128(0xca02)),
            spawn_payload,
        )?;
        state.apply_effect_transactionally(&spawn, |_| GuardianEffectOutcome::<()>::Applied)?;
        let claim = checkpoint_catalog_authenticate_request(
            GuardianOperation::Claim,
            guardian_incarnation,
            mux_incarnation,
            Uuid::from_u128(0xca03),
            Some(pane_id),
            0,
            0,
            Some(Uuid::from_u128(0xca04)),
            Zeroizing::new(Vec::new()),
        )?;
        state.apply_effect_transactionally(&claim, |_| GuardianEffectOutcome::<()>::Applied)?;
        Ok(state)
    }

    #[allow(clippy::too_many_arguments)]
    fn checkpoint_catalog_test_adoption_request(
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
        request_id: Uuid,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        effect_id: Uuid,
        intent: GuardianCheckpointIntent,
    ) -> Result<AuthenticatedGuardianRequest, GuardianProtocolError> {
        let mut payload = Zeroizing::new(Vec::new());
        payload.extend_from_slice(&intent.encode());
        checkpoint_catalog_authenticate_request(
            GuardianOperation::Checkpoint,
            guardian_incarnation,
            mux_incarnation,
            request_id,
            Some(pane_id),
            generation,
            sequence,
            Some(effect_id),
            payload,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn checkpoint_catalog_issue_test_adoption_seed(
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
        request_id: Uuid,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        effect_id: Uuid,
        intent: GuardianCheckpointIntent,
    ) -> Result<GuardianCheckpointCatalogAdoptionEvidenceSeedV1, Box<dyn std::error::Error>> {
        let mut state = checkpoint_catalog_claimed_protocol_state(
            guardian_incarnation,
            mux_incarnation,
            pane_id,
        )?;
        let request = checkpoint_catalog_test_adoption_request(
            guardian_incarnation,
            mux_incarnation,
            request_id,
            pane_id,
            generation,
            sequence,
            effect_id,
            intent,
        )?;
        let mut seed = None;
        let receipt = state.apply_checkpoint_transactionally(&request, |permit| {
            seed = Some(permit.into_evidence_seed());
            Ok::<(), ()>(())
        })?;
        if receipt.disposition() != mux::guardian_protocol::GuardianCheckpointDisposition::Committed
        {
            return Err("test adoption seed transaction did not commit".into());
        }
        match seed {
            Some(seed) => Ok(seed),
            None => Err("test adoption transaction did not issue its single-use seed".into()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn checkpoint_catalog_publish_test_adoption(
        store: &GuardianCheckpointStageStore,
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
        request_id: Uuid,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        effect_id: Uuid,
        intent: GuardianCheckpointIntent,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut state = checkpoint_catalog_claimed_protocol_state(
            guardian_incarnation,
            mux_incarnation,
            pane_id,
        )?;
        let request = checkpoint_catalog_test_adoption_request(
            guardian_incarnation,
            mux_incarnation,
            request_id,
            pane_id,
            generation,
            sequence,
            effect_id,
            intent,
        )?;
        let receipt = state.apply_checkpoint_transactionally(&request, |permit| {
            store.publish_checkpoint_catalog_adoption(permit)
        })?;
        if receipt.disposition() != mux::guardian_protocol::GuardianCheckpointDisposition::Committed
        {
            return Err("test catalog adoption publication did not commit".into());
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn checkpoint_catalog_test_record_stage_request(
        kind: GuardianCheckpointStageKindV1,
        pane_id: Uuid,
        generation: u64,
        upload_id: Uuid,
        terminal: &RecoveryTerminalCheckpointV2,
        receipt: GuardianOutputAppendReceipt,
        chunk_bytes: u32,
        chunk: Option<(u32, &[u8])>,
    ) -> Result<GuardianCheckpointStageRequestV1, GuardianProtocolError> {
        let canonical_payload = terminal.canonical_payload();
        let total_bytes = u64::try_from(canonical_payload.len())
            .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?;
        let total_chunks = u32::try_from(total_bytes.div_ceil(u64::from(chunk_bytes)))
            .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?;
        let parser_stream_bytes = terminal.parser_stream_bytes();
        let replay_identity = mux::guardian_checkpoint::current_replay_identity_digest();

        let terminal_digest = checkpoint_test_terminal_digest(canonical_payload);
        let mut boundary_hasher = Sha256::new();
        boundary_hasher.update(b"frankenterm.guardian-checkpoint-output-boundary-identity.v1\0");
        boundary_hasher.update(2_u32.to_le_bytes());
        boundary_hasher.update(pane_id.as_bytes());
        boundary_hasher.update(receipt.segment_id().as_bytes());
        boundary_hasher.update(receipt.sequence().to_le_bytes());
        boundary_hasher.update(receipt.record_digest());
        boundary_hasher.update(receipt.committed_log_bytes().to_le_bytes());
        boundary_hasher.update(receipt.cumulative_plaintext_bytes().to_le_bytes());
        let boundary_digest: [u8; 32] = boundary_hasher.finalize().into();

        let mut checkpoint_hasher = Sha256::new();
        checkpoint_hasher.update(b"frankenterm.guardian-checkpoint-artifact-identity.v1\0");
        checkpoint_hasher.update(boundary_digest);
        checkpoint_hasher.update(parser_stream_bytes.to_le_bytes());
        checkpoint_hasher.update(replay_identity);
        checkpoint_hasher.update(24_u32.to_le_bytes());
        checkpoint_hasher.update(80_u32.to_le_bytes());
        checkpoint_hasher.update(total_bytes.to_le_bytes());
        checkpoint_hasher.update(terminal_digest);
        let checkpoint_digest: [u8; 32] = checkpoint_hasher.finalize().into();

        let trailing_bytes = chunk.map_or(0, |(_, bytes)| 48 + bytes.len());
        let mut wire = Zeroizing::new(Vec::new());
        wire.try_reserve_exact(
            CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES
                .checked_add(trailing_bytes)
                .ok_or(GuardianProtocolError::PayloadTooLarge)?,
        )
        .map_err(|_| GuardianProtocolError::PayloadTooLarge)?;
        wire.resize(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES, 0);
        wire[..4].copy_from_slice(b"GCS1");
        wire[4..6].copy_from_slice(&2_u16.to_be_bytes());
        wire[6] = match kind {
            GuardianCheckpointStageKindV1::Begin => 1,
            GuardianCheckpointStageKindV1::Chunk => 2,
            GuardianCheckpointStageKindV1::Seal => 3,
            GuardianCheckpointStageKindV1::Query => 4,
            GuardianCheckpointStageKindV1::Ack => {
                return Err(GuardianProtocolError::InvalidOperationPayload);
            }
        };
        wire[8] = 1;
        wire[16..32].copy_from_slice(pane_id.as_bytes());
        wire[32..40].copy_from_slice(&generation.to_be_bytes());
        wire[40..56].copy_from_slice(upload_id.as_bytes());
        wire[56..88].copy_from_slice(&checkpoint_digest);
        wire[88..120].copy_from_slice(&boundary_digest);
        wire[120..128].copy_from_slice(&generation.to_be_bytes());
        wire[128..160].copy_from_slice(&replay_identity);
        wire[160..164].copy_from_slice(&24_u32.to_be_bytes());
        wire[164..168].copy_from_slice(&80_u32.to_be_bytes());
        wire[168..176].copy_from_slice(&total_bytes.to_be_bytes());
        wire[176..208].copy_from_slice(&terminal_digest);
        wire[208..224].copy_from_slice(pane_id.as_bytes());
        wire[224] = 2;
        wire[248..264].copy_from_slice(receipt.segment_id().as_bytes());
        wire[264..272].copy_from_slice(&receipt.sequence().to_be_bytes());
        wire[272..304].copy_from_slice(&receipt.record_digest());
        wire[304..312].copy_from_slice(&receipt.committed_log_bytes().to_be_bytes());
        wire[312..320].copy_from_slice(&receipt.cumulative_plaintext_bytes().to_be_bytes());
        wire[320..328].copy_from_slice(&parser_stream_bytes.to_be_bytes());
        wire[328..332].copy_from_slice(&chunk_bytes.to_be_bytes());
        wire[332..336].copy_from_slice(&total_chunks.to_be_bytes());
        if let Some((index, bytes)) = chunk {
            let offset = u64::from(index)
                .checked_mul(u64::from(chunk_bytes))
                .ok_or(GuardianProtocolError::InvalidOperationPayload)?;
            let chunk_digest: [u8; 32] = Sha256::digest(bytes).into();
            wire.extend_from_slice(&index.to_be_bytes());
            wire.extend_from_slice(&offset.to_be_bytes());
            wire.extend_from_slice(&chunk_digest);
            wire.extend_from_slice(
                &u32::try_from(bytes.len())
                    .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?
                    .to_be_bytes(),
            );
            wire.extend_from_slice(bytes);
        }
        GuardianCheckpointStageRequestV1::decode(&wire)
    }

    #[allow(clippy::too_many_arguments)]
    fn checkpoint_catalog_stage_and_publish(
        pipeline: &GuardianOutputPipeline,
        store: &GuardianCheckpointStageStore,
        journal: &GuardianPaneOutputJournal,
        state: &mut GuardianProtocolState,
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        identity_base: u128,
        appended_output: &[u8],
        complete_terminal_output: &[u8],
    ) -> Result<([u8; 32], [u8; 32]), Box<dyn std::error::Error>> {
        let receipt = durable_commit(pipeline, pane_id, journal, appended_output)?;
        let terminal = checkpoint_catalog_test_terminal(complete_terminal_output);
        if terminal.parser_stream_bytes() != receipt.cumulative_plaintext_bytes() {
            return Err("catalog fixture parser/output watermark mismatch".into());
        }
        let upload_id = Uuid::from_u128(identity_base);
        let chunk_bytes = 1_024_u32;
        let begin = checkpoint_catalog_test_record_stage_request(
            GuardianCheckpointStageKindV1::Begin,
            pane_id,
            generation,
            upload_id,
            &terminal,
            receipt,
            chunk_bytes,
            None,
        )?;
        let checkpoint_id = begin.checkpoint_id().into_bytes();
        let boundary_id = begin.boundary_id().into_bytes();
        let replay_semantics_id = begin.descriptor().replay_semantics_id();
        store.apply_begin(&begin)?;
        for (index, bytes) in terminal
            .canonical_payload()
            .chunks(usize::try_from(chunk_bytes)?)
            .enumerate()
        {
            store.apply_chunk(checkpoint_catalog_test_record_stage_request(
                GuardianCheckpointStageKindV1::Chunk,
                pane_id,
                generation,
                upload_id,
                &terminal,
                receipt,
                chunk_bytes,
                Some((u32::try_from(index)?, bytes)),
            )?)?;
        }

        let seal = checkpoint_catalog_test_record_stage_request(
            GuardianCheckpointStageKindV1::Seal,
            pane_id,
            generation,
            upload_id,
            &terminal,
            receipt,
            chunk_bytes,
            None,
        )?;
        let authenticated_seal = checkpoint_catalog_authenticate_request(
            GuardianOperation::CheckpointStage,
            guardian_incarnation,
            mux_incarnation,
            Uuid::from_u128(identity_base + 1),
            Some(pane_id),
            generation,
            0,
            None,
            seal.into_zeroizing_payload()?,
        )?;
        let seal_permit = state.preflight_checkpoint_seal(&authenticated_seal)?;
        store.apply_runtime_seal(seal_permit, journal)?;

        let intent = GuardianCheckpointIntent::new(begin.checkpoint_id(), begin.boundary_id());
        let mut adoption_payload = Zeroizing::new(Vec::new());
        adoption_payload.extend_from_slice(&intent.encode());
        let adoption = checkpoint_catalog_authenticate_request(
            GuardianOperation::Checkpoint,
            guardian_incarnation,
            mux_incarnation,
            Uuid::from_u128(identity_base + 2),
            Some(pane_id),
            generation,
            sequence,
            Some(Uuid::from_u128(identity_base + 3)),
            adoption_payload,
        )?;
        state.apply_checkpoint_transactionally(&adoption, |permit| {
            store.publish_checkpoint_catalog_adoption(permit)
        })?;
        Ok((checkpoint_id, replay_semantics_id))
    }

    fn checkpoint_catalog_rewrite_protected_member_as_legacy_fixture(
        inner: &GuardianCheckpointStageStoreInner,
        member: &PublishedCheckpointCatalogMember,
    ) -> Result<(Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
        if member.format != CheckpointCatalogFormat::ProtectedV3 {
            return Err("fixture source member is not protected v3".into());
        }
        let mut candidate_bytes = std::fs::read(&member.candidate_path)?;
        let evidence_bytes = usize::try_from(CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_RECORD_BYTES)?;
        let legacy_len = candidate_bytes
            .len()
            .checked_sub(evidence_bytes)
            .ok_or("protected candidate is shorter than its evidence record")?;
        candidate_bytes.truncate(legacy_len);
        candidate_bytes[..8].copy_from_slice(&CHECKPOINT_CATALOG_LEGACY_CANDIDATE_MAGIC);
        candidate_bytes[8..12].copy_from_slice(&CHECKPOINT_CATALOG_LEGACY_VERSION.to_le_bytes());
        candidate_bytes[CHECKPOINT_CATALOG_HEADER_BYTES - 12..CHECKPOINT_CATALOG_HEADER_BYTES]
            .fill(0);
        let checksum_offset = candidate_bytes
            .len()
            .checked_sub(OUTPUT_MANIFEST_CHECKSUM_BYTES)
            .ok_or("legacy candidate checksum offset")?;
        let checksum = checkpoint_catalog_checksum(
            CHECKPOINT_CATALOG_LEGACY_CHECKSUM_DOMAIN,
            &candidate_bytes[..checksum_offset],
        );
        candidate_bytes[checksum_offset..].copy_from_slice(&checksum);
        let decoded = checkpoint_catalog_decode_candidate(&candidate_bytes)?;
        if decoded.format != CheckpointCatalogFormat::LegacyV2
            || decoded.metadata != member.metadata
            || decoded.adoption_evidence.is_some()
        {
            return Err("converted legacy candidate is not exact".into());
        }
        let marker_bytes =
            checkpoint_catalog_encode_marker(&checkpoint_catalog_marker_for_candidate(&decoded));
        let decoded_marker = checkpoint_catalog_decode_marker(&marker_bytes)?;
        if decoded_marker.format != CheckpointCatalogFormat::LegacyV2
            || decoded_marker.identity != member.metadata.identity
            || decoded_marker.candidate_checksum != checksum
        {
            return Err("converted legacy marker is not exact".into());
        }

        let mut candidate_file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&member.candidate_path)?;
        candidate_file.write_all(&candidate_bytes)?;
        candidate_file.sync_all()?;
        let mut marker_file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&member.marker_path)?;
        marker_file.write_all(&marker_bytes)?;
        marker_file.sync_all()?;
        inner.directory.sync_all()?;
        Ok((candidate_bytes, marker_bytes))
    }

    fn checkpoint_test_genesis_request(
        kind: GuardianCheckpointStageKindV1,
        spawn_effect_id: Uuid,
        upload_id: Uuid,
        terminal_payload: &[u8],
        chunk_bytes: u32,
        chunk: Option<(u32, &[u8])>,
    ) -> Result<GuardianCheckpointStageRequestV1, GuardianProtocolError> {
        if chunk_bytes == 0 {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let total_bytes = u64::try_from(terminal_payload.len())
            .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?;
        let total_chunks_u64 = total_bytes.div_ceil(u64::from(chunk_bytes));
        let total_chunks = u32::try_from(total_chunks_u64)
            .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?;
        let replay_identity = mux::guardian_checkpoint::current_replay_identity_digest();
        let terminal_digest = checkpoint_test_terminal_digest(terminal_payload);
        let mut boundary_hasher = Sha256::new();
        boundary_hasher.update(b"frankenterm.guardian-checkpoint-genesis-boundary-identity.v1\0");
        boundary_hasher.update(spawn_effect_id.as_bytes());
        let boundary_digest: [u8; 32] = boundary_hasher.finalize().into();
        let mut checkpoint_hasher = Sha256::new();
        checkpoint_hasher.update(b"frankenterm.guardian-checkpoint-artifact-identity.v1\0");
        checkpoint_hasher.update(boundary_digest);
        checkpoint_hasher.update(0_u64.to_le_bytes());
        checkpoint_hasher.update(replay_identity);
        checkpoint_hasher.update(24_u32.to_le_bytes());
        checkpoint_hasher.update(80_u32.to_le_bytes());
        checkpoint_hasher.update(total_bytes.to_le_bytes());
        checkpoint_hasher.update(terminal_digest);
        let checkpoint_digest: [u8; 32] = checkpoint_hasher.finalize().into();

        let trailing_bytes = match (kind, chunk) {
            (GuardianCheckpointStageKindV1::Chunk, Some((_, bytes))) => 48_usize
                .checked_add(bytes.len())
                .ok_or(GuardianProtocolError::PayloadTooLarge)?,
            (
                GuardianCheckpointStageKindV1::Begin
                | GuardianCheckpointStageKindV1::Seal
                | GuardianCheckpointStageKindV1::Query,
                None,
            ) => 0,
            _ => return Err(GuardianProtocolError::InvalidOperationPayload),
        };
        let wire_bytes = CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES
            .checked_add(trailing_bytes)
            .ok_or(GuardianProtocolError::PayloadTooLarge)?;
        let mut wire: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::with_capacity(wire_bytes));
        wire.resize(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES, 0);
        wire[..4].copy_from_slice(b"GCS1");
        wire[4..6].copy_from_slice(&2_u16.to_be_bytes());
        wire[6] = match kind {
            GuardianCheckpointStageKindV1::Begin => 1,
            GuardianCheckpointStageKindV1::Chunk => 2,
            GuardianCheckpointStageKindV1::Seal => 3,
            GuardianCheckpointStageKindV1::Query => 4,
            GuardianCheckpointStageKindV1::Ack => {
                return Err(GuardianProtocolError::InvalidOperationPayload);
            }
        };
        wire[8] = 2;
        wire[16..32].copy_from_slice(spawn_effect_id.as_bytes());
        wire[40..56].copy_from_slice(upload_id.as_bytes());
        wire[56..88].copy_from_slice(&checkpoint_digest);
        wire[88..120].copy_from_slice(&boundary_digest);
        wire[120..128].copy_from_slice(&1_u64.to_be_bytes());
        wire[128..160].copy_from_slice(&replay_identity);
        wire[160..164].copy_from_slice(&24_u32.to_be_bytes());
        wire[164..168].copy_from_slice(&80_u32.to_be_bytes());
        wire[168..176].copy_from_slice(&total_bytes.to_be_bytes());
        wire[176..208].copy_from_slice(&terminal_digest);
        wire[224] = 1;
        wire[232..248].copy_from_slice(spawn_effect_id.as_bytes());
        wire[328..332].copy_from_slice(&chunk_bytes.to_be_bytes());
        wire[332..336].copy_from_slice(&total_chunks.to_be_bytes());
        match (kind, chunk) {
            (GuardianCheckpointStageKindV1::Chunk, Some((index, bytes))) => {
                let offset = u64::from(index)
                    .checked_mul(u64::from(chunk_bytes))
                    .ok_or(GuardianProtocolError::InvalidOperationPayload)?;
                let digest = checkpoint_zeroizing_sha256_digest(bytes);
                wire.extend_from_slice(&index.to_be_bytes());
                wire.extend_from_slice(&offset.to_be_bytes());
                wire.extend_from_slice(digest.as_slice());
                wire.extend_from_slice(
                    &u32::try_from(bytes.len())
                        .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?
                        .to_be_bytes(),
                );
                wire.extend_from_slice(bytes);
            }
            (
                GuardianCheckpointStageKindV1::Begin
                | GuardianCheckpointStageKindV1::Seal
                | GuardianCheckpointStageKindV1::Query,
                None,
            ) => {}
            _ => unreachable!("checkpoint Stage shape was validated before allocation"),
        }
        debug_assert_eq!(wire.len(), wire_bytes);
        GuardianCheckpointStageRequestV1::decode(&wire)
    }

    #[test]
    fn checkpoint_stage_name_grammar_is_canonical_raw_and_bounded() {
        let pane_id = Uuid::from_u128(0xabcdef1234567890abcdef1234567890);
        let upload_id = Uuid::from_u128(0x22);
        let publication_id = Uuid::from_u128(0x33);
        let key = CheckpointStageUploadKey {
            scope: CheckpointStagePathScope::Pane {
                pane_id,
                generation: 7,
            },
            upload_id,
        };
        let chunk_name = format!(
            "{}.publication-{publication_id}.chunk-{:010}{CHECKPOINT_STAGE_FILE_SUFFIX}",
            key.base_name(),
            9,
        );
        assert_eq!(
            checkpoint_parse_stage_name(chunk_name.as_bytes()).expect("parse canonical chunk"),
            (
                key,
                CheckpointStageFileRole::Chunk {
                    publication_id,
                    index: 9,
                },
            )
        );
        assert_eq!(checkpoint_stage_longest_name_bytes(), 200);

        let ack_name = format!(
            "{}.publication-{publication_id}.ack{CHECKPOINT_STAGE_FILE_SUFFIX}",
            key.base_name(),
        );
        assert_eq!(
            checkpoint_parse_stage_name(ack_name.as_bytes()).expect("parse canonical ACK"),
            (key, CheckpointStageFileRole::Ack { publication_id },)
        );
        let expired_name = format!(
            "{}.publication-{publication_id}.expired{CHECKPOINT_STAGE_FILE_SUFFIX}",
            key.base_name(),
        );
        assert_eq!(
            checkpoint_parse_stage_name(expired_name.as_bytes())
                .expect("parse canonical expiry finalizer"),
            (key, CheckpointStageFileRole::Expired { publication_id },)
        );

        let uppercase =
            chunk_name.replace(&pane_id.to_string(), &pane_id.to_string().to_uppercase());
        assert!(matches!(
            checkpoint_parse_stage_name(uppercase.as_bytes()),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        let short_generation =
            chunk_name.replace("generation-00000000000000000007", "generation-7");
        assert!(matches!(
            checkpoint_parse_stage_name(short_generation.as_bytes()),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        let excessive_index = chunk_name.replace("chunk-0000000009", "chunk-0000001024");
        assert!(matches!(
            checkpoint_parse_stage_name(excessive_index.as_bytes()),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        let mut invalid_utf8 = b"checkpoint-pane-".to_vec();
        invalid_utf8.push(0xff);
        assert!(matches!(
            checkpoint_parse_stage_name(&invalid_utf8),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
    }

    fn checkpoint_catalog_test_marker(
        identity: CheckpointCatalogIdentity,
    ) -> CheckpointCatalogMarker {
        let (adoption_effect_id, adoption_sequence) = match identity.scope {
            CheckpointCatalogScope::Pane { .. } => (Uuid::from_u128(0xc004), 1),
            CheckpointCatalogScope::Genesis { spawn_effect_id } => (spawn_effect_id, 0),
        };
        CheckpointCatalogMarker {
            format: CheckpointCatalogFormat::ProtectedV3,
            identity,
            predecessor_generation: None,
            predecessor_candidate_id: Uuid::nil(),
            predecessor_checksum: [0; 32],
            upload_id: Uuid::from_u128(0xc001),
            completion_id: Uuid::from_u128(0xc002),
            checkpoint_id: [0x31; 32],
            boundary_id: [0x32; 32],
            terminal_payload_digest: [0x33; 32],
            candidate_checksum: [0x34; 32],
            adoption_mux_incarnation: Uuid::from_u128(0xc003),
            adoption_effect_id,
            adoption_sequence,
        }
    }

    fn checkpoint_catalog_test_genesis_reservation_binding()
    -> CheckpointCatalogGenesisReservationBinding {
        CheckpointCatalogGenesisReservationBinding {
            mux_incarnation: Uuid::from_u128(0xe001),
            spawn_effect_id: Uuid::from_u128(0xe002),
            durable_pane_id: Uuid::from_u128(0xe003),
            origin_request_id: Uuid::from_u128(0xe004),
            spawn_payload_bytes: 73,
            spawn_payload_digest: [0x51; 32],
            spawning_mux_build_identity_digest: [0x52; 32],
            live_guardian_build_identity_digest: [0x53; 32],
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 480,
            checkpoint_identity_digest: [0x54; 32],
            boundary_identity_digest: [0x55; 32],
            upload_id: Uuid::from_u128(0xe005),
        }
    }

    fn checkpoint_catalog_test_genesis_metadata(
        reservation: CheckpointCatalogGenesisReservationBinding,
    ) -> CheckpointCatalogMetadata {
        CheckpointCatalogMetadata {
            identity: CheckpointCatalogIdentity {
                scope: CheckpointCatalogScope::Genesis {
                    spawn_effect_id: reservation.spawn_effect_id,
                },
                generation: 1,
                candidate_id: checkpoint_catalog_genesis_candidate_id(reservation),
            },
            predecessor: None,
            upload_id: reservation.upload_id,
            completion_id: Uuid::from_u128(0xe006),
            checkpoint_id: reservation.checkpoint_identity_digest,
            boundary_id: reservation.boundary_identity_digest,
            terminal_payload_digest: [0x55; 32],
            total_bytes: 17,
            chunk_count: 1,
            capture_generation: 1,
            replay_semantics_id: [0x56; 32],
            rows: u32::from(reservation.rows),
            cols: u32::from(reservation.cols),
            adoption_mux_incarnation: reservation.mux_incarnation,
            adoption_effect_id: reservation.spawn_effect_id,
            adoption_sequence: 0,
            genesis_durable_pane_id: reservation.durable_pane_id,
            genesis_origin_request_id: reservation.origin_request_id,
            genesis_spawn_payload_bytes: reservation.spawn_payload_bytes,
            genesis_spawn_payload_digest: reservation.spawn_payload_digest,
            genesis_spawning_mux_build_identity_digest: reservation
                .spawning_mux_build_identity_digest,
            genesis_live_guardian_build_identity_digest: reservation
                .live_guardian_build_identity_digest,
            genesis_pixel_width: reservation.pixel_width,
            genesis_pixel_height: reservation.pixel_height,
        }
    }

    #[test]
    fn checkpoint_catalog_genesis_metadata_is_scope_specific_and_reservation_complete() {
        let reservation = checkpoint_catalog_test_genesis_reservation_binding();
        let genesis = checkpoint_catalog_test_genesis_metadata(reservation);
        checkpoint_catalog_validate_metadata(&genesis).expect("valid Genesis metadata");
        assert!(checkpoint_catalog_genesis_metadata_matches_reservation(
            &genesis,
            reservation
        ));

        let pane_scope = CheckpointCatalogScope::Pane {
            pane_id: Uuid::from_u128(0xe100),
        };
        let pane = checkpoint_catalog_test_member(pane_scope, 1, None, 0x21).metadata;
        checkpoint_catalog_validate_metadata(&pane).expect("valid Pane metadata");
        let mut pane_with_zero_sequence = pane;
        pane_with_zero_sequence.adoption_sequence = 0;
        assert!(checkpoint_catalog_validate_metadata(&pane_with_zero_sequence).is_err());
        let mut pane_with_genesis_field = pane;
        pane_with_genesis_field.genesis_durable_pane_id = reservation.durable_pane_id;
        assert!(checkpoint_catalog_validate_metadata(&pane_with_genesis_field).is_err());

        let mut wrong_generation = genesis;
        wrong_generation.identity.generation = 2;
        assert!(checkpoint_catalog_validate_metadata(&wrong_generation).is_err());
        let mut wrong_capture_generation = genesis;
        wrong_capture_generation.capture_generation = 2;
        assert!(checkpoint_catalog_validate_metadata(&wrong_capture_generation).is_err());
        let mut predecessor = genesis;
        predecessor.predecessor = Some(CheckpointCatalogPredecessor {
            generation: 1,
            candidate_id: Uuid::from_u128(0xe101),
            candidate_checksum: [0x61; 32],
            checkpoint_id: [0x62; 32],
            boundary_id: [0x63; 32],
        });
        predecessor.identity.generation = 2;
        assert!(checkpoint_catalog_validate_metadata(&predecessor).is_err());
        let mut nonzero_sequence = genesis;
        nonzero_sequence.adoption_sequence = 1;
        assert!(checkpoint_catalog_validate_metadata(&nonzero_sequence).is_err());
        let mut wrong_spawn = genesis;
        wrong_spawn.adoption_effect_id = Uuid::from_u128(0xe102);
        assert!(checkpoint_catalog_validate_metadata(&wrong_spawn).is_err());

        let mut mutations = Vec::new();
        let mut changed = genesis;
        changed.upload_id = Uuid::from_u128(0xe110);
        mutations.push(changed);
        changed = genesis;
        changed.checkpoint_id[0] ^= 1;
        mutations.push(changed);
        changed = genesis;
        changed.boundary_id[0] ^= 1;
        mutations.push(changed);
        changed = genesis;
        changed.rows += 1;
        mutations.push(changed);
        changed = genesis;
        changed.cols += 1;
        mutations.push(changed);
        changed = genesis;
        changed.adoption_mux_incarnation = Uuid::from_u128(0xe111);
        mutations.push(changed);
        changed = genesis;
        changed.genesis_durable_pane_id = Uuid::from_u128(0xe112);
        mutations.push(changed);
        changed = genesis;
        changed.genesis_origin_request_id = Uuid::from_u128(0xe113);
        mutations.push(changed);
        changed = genesis;
        changed.genesis_spawn_payload_bytes += 1;
        mutations.push(changed);
        changed = genesis;
        changed.genesis_spawn_payload_digest[0] ^= 1;
        mutations.push(changed);
        changed = genesis;
        changed.genesis_spawning_mux_build_identity_digest[0] ^= 1;
        mutations.push(changed);
        changed = genesis;
        changed.genesis_live_guardian_build_identity_digest[0] ^= 1;
        mutations.push(changed);
        changed = genesis;
        changed.genesis_pixel_width += 1;
        mutations.push(changed);
        changed = genesis;
        changed.genesis_pixel_height += 1;
        mutations.push(changed);
        for mutation in mutations {
            assert!(
                !checkpoint_catalog_genesis_metadata_matches_reservation(&mutation, reservation),
                "every reservation field must remain catalog-bound"
            );
        }
    }

    #[test]
    fn checkpoint_catalog_genesis_candidate_identity_binds_every_reservation_field() {
        let reservation = checkpoint_catalog_test_genesis_reservation_binding();
        let expected = checkpoint_catalog_genesis_candidate_id(reservation);
        assert!(!expected.is_nil());
        let mut mutations = Vec::new();
        let mut changed = reservation;
        changed.mux_incarnation = Uuid::from_u128(0xe201);
        mutations.push(changed);
        changed = reservation;
        changed.spawn_effect_id = Uuid::from_u128(0xe202);
        mutations.push(changed);
        changed = reservation;
        changed.durable_pane_id = Uuid::from_u128(0xe203);
        mutations.push(changed);
        changed = reservation;
        changed.origin_request_id = Uuid::from_u128(0xe204);
        mutations.push(changed);
        changed = reservation;
        changed.spawn_payload_bytes += 1;
        mutations.push(changed);
        changed = reservation;
        changed.spawn_payload_digest[0] ^= 1;
        mutations.push(changed);
        changed = reservation;
        changed.spawning_mux_build_identity_digest[0] ^= 1;
        mutations.push(changed);
        changed = reservation;
        changed.live_guardian_build_identity_digest[0] ^= 1;
        mutations.push(changed);
        changed = reservation;
        changed.rows += 1;
        mutations.push(changed);
        changed = reservation;
        changed.cols += 1;
        mutations.push(changed);
        changed = reservation;
        changed.pixel_width += 1;
        mutations.push(changed);
        changed = reservation;
        changed.pixel_height += 1;
        mutations.push(changed);
        changed = reservation;
        changed.checkpoint_identity_digest[0] ^= 1;
        mutations.push(changed);
        changed = reservation;
        changed.boundary_identity_digest[0] ^= 1;
        mutations.push(changed);
        changed = reservation;
        changed.upload_id = Uuid::from_u128(0xe205);
        mutations.push(changed);
        for mutation in mutations {
            assert_ne!(checkpoint_catalog_genesis_candidate_id(mutation), expected);
        }
    }

    #[test]
    fn checkpoint_catalog_name_grammar_is_canonical_and_bounded() {
        let identity = CheckpointCatalogIdentity {
            scope: CheckpointCatalogScope::Pane {
                pane_id: Uuid::from_u128(0xc1),
            },
            generation: 7,
            candidate_id: Uuid::from_u128(0xc2),
        };
        for role in [
            CheckpointCatalogPathRole::Candidate,
            CheckpointCatalogPathRole::Marker,
        ] {
            let suffix = match role {
                CheckpointCatalogPathRole::Candidate => CHECKPOINT_CATALOG_CANDIDATE_SUFFIX,
                CheckpointCatalogPathRole::Marker => CHECKPOINT_CATALOG_MARKER_SUFFIX,
            };
            let name = format!("{}{suffix}", checkpoint_catalog_base_name(identity));
            assert_eq!(
                checkpoint_catalog_parse_path(&name),
                Some((identity, CheckpointCatalogPathKind::Canonical(role)))
            );
            let staging = format!("{name}{CHECKPOINT_CATALOG_STAGING_SUFFIX}");
            assert_eq!(
                checkpoint_catalog_parse_path(&staging),
                Some((identity, CheckpointCatalogPathKind::Staging(role)))
            );
            assert!(checkpoint_catalog_parse_path(&name.to_uppercase()).is_none());
            assert!(checkpoint_catalog_parse_path(&staging.to_uppercase()).is_none());
        }
        assert!(checkpoint_catalog_longest_name_bytes() <= 255);
    }

    #[test]
    fn checkpoint_catalog_publication_resumes_torn_staging_prefix_and_lost_reply()
    -> Result<(), Box<dyn std::error::Error>> {
        let expected = b"complete-checkpoint-catalog-publication";
        for (case, prefix_bytes) in [0_usize, 7, expected.len()].into_iter().enumerate() {
            let case_id = u128::try_from(case)?;
            let directory_prefix = format!("ft-guardian-catalog-stage-resume-{case}-");
            let (_directory, _poll, pipeline) =
                pipeline_with_policy(&directory_prefix, OutputSegmentPolicy::production())?;
            let store = pipeline.checkpoint_stage_store();
            let scope = CheckpointCatalogScope::Pane {
                pane_id: Uuid::from_u128(0xc300 + case_id),
            };
            let identity = CheckpointCatalogIdentity {
                scope,
                generation: 1,
                candidate_id: Uuid::from_u128(0xc400 + case_id),
            };
            let canonical = checkpoint_catalog_path(
                &store.inner,
                identity,
                CheckpointCatalogPathRole::Candidate,
            )?;
            let staging = checkpoint_catalog_staging_path(&store.inner, &canonical)?;
            let mut torn = create_private_file_new_at(
                &store.inner.directory,
                &store.inner.directory_path,
                &staging,
            )?;
            torn.write_all(&expected[..prefix_bytes])?;
            torn.sync_all()?;
            store.inner.directory.sync_all()?;

            let before = checkpoint_catalog_scan(&store.inner, scope)?;
            assert!(before.published.is_empty());
            assert!(before.unpublished_candidates.is_empty());
            assert!(matches!(
                std::fs::symlink_metadata(&canonical),
                Err(error) if error.kind() == ErrorKind::NotFound
            ));
            assert_eq!(before.staged_files.len(), 1);
            assert_eq!(before.staged_files[0].identity, identity);
            assert_eq!(
                before.staged_files[0].role,
                CheckpointCatalogPathRole::Candidate
            );
            assert_eq!(before.staged_files[0].bytes, u64::try_from(prefix_bytes)?);

            let first = checkpoint_catalog_publish_file(
                &store.inner,
                &canonical,
                expected,
                "checkpoint-catalog-test-resume-write",
                "checkpoint-catalog-test-resume-sync",
            )?;
            assert_eq!(std::fs::read(&canonical)?, expected);
            assert!(matches!(
                std::fs::symlink_metadata(&staging),
                Err(error) if error.kind() == ErrorKind::NotFound
            ));

            let recovered_lost_reply = checkpoint_catalog_publish_file(
                &store.inner,
                &canonical,
                expected,
                "checkpoint-catalog-test-lost-reply-write",
                "checkpoint-catalog-test-lost-reply-sync",
            )?;
            assert_eq!(recovered_lost_reply, first);
            assert_eq!(std::fs::read(&canonical)?, expected);
        }
        Ok(())
    }

    #[test]
    fn checkpoint_catalog_publication_resumes_every_adoption_evidence_boundary()
    -> Result<(), Box<dyn std::error::Error>> {
        for case in 0_u128..7 {
            let directory_prefix = format!("ft-guardian-catalog-evidence-prefix-{case}-");
            let (_directory, _poll, pipeline) =
                pipeline_with_policy(&directory_prefix, OutputSegmentPolicy::production())?;
            let store = pipeline.checkpoint_stage_store();
            let identity_base = 0xc4_0000_u128
                .checked_add(case.checked_mul(0x100).ok_or("case identity overflow")?)
                .ok_or("case identity overflow")?;
            let guardian_incarnation = Uuid::from_u128(identity_base + 1);
            let mux_incarnation = Uuid::from_u128(identity_base + 2);
            let pane_id = Uuid::from_u128(identity_base + 3);
            let generation = 1;
            let sequence = 1;
            let upload_id = Uuid::from_u128(identity_base + 4);
            let seal_request_id = Uuid::from_u128(identity_base + 5);
            let adoption_request_id = Uuid::from_u128(identity_base + 6);
            let adoption_effect_id = Uuid::from_u128(identity_base + 7);

            let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
            let receipt = durable_commit(&pipeline, pane_id, &journal, b"stable-evidence")?;
            let terminal = checkpoint_catalog_test_terminal(b"stable-evidence");
            assert_eq!(
                terminal.parser_stream_bytes(),
                receipt.cumulative_plaintext_bytes()
            );
            let chunk_bytes = 1_024_u32;
            let begin = checkpoint_catalog_test_record_stage_request(
                GuardianCheckpointStageKindV1::Begin,
                pane_id,
                generation,
                upload_id,
                &terminal,
                receipt,
                chunk_bytes,
                None,
            )?;
            store.apply_begin(&begin)?;
            for (index, bytes) in terminal
                .canonical_payload()
                .chunks(usize::try_from(chunk_bytes)?)
                .enumerate()
            {
                store.apply_chunk(checkpoint_catalog_test_record_stage_request(
                    GuardianCheckpointStageKindV1::Chunk,
                    pane_id,
                    generation,
                    upload_id,
                    &terminal,
                    receipt,
                    chunk_bytes,
                    Some((u32::try_from(index)?, bytes)),
                )?)?;
            }
            let seal_for_preflight = checkpoint_catalog_test_record_stage_request(
                GuardianCheckpointStageKindV1::Seal,
                pane_id,
                generation,
                upload_id,
                &terminal,
                receipt,
                chunk_bytes,
                None,
            )?;
            let authenticated_seal = checkpoint_catalog_authenticate_request(
                GuardianOperation::CheckpointStage,
                guardian_incarnation,
                mux_incarnation,
                seal_request_id,
                Some(pane_id),
                generation,
                0,
                None,
                seal_for_preflight.into_zeroizing_payload()?,
            )?;
            let seal_state = checkpoint_catalog_claimed_protocol_state(
                guardian_incarnation,
                mux_incarnation,
                pane_id,
            )?;
            let seal_permit = seal_state.preflight_checkpoint_seal(&authenticated_seal)?;
            store.apply_runtime_seal(seal_permit, &journal)?;

            let seal = checkpoint_catalog_test_record_stage_request(
                GuardianCheckpointStageKindV1::Seal,
                pane_id,
                generation,
                upload_id,
                &terminal,
                receipt,
                chunk_bytes,
                None,
            )?;
            let intent = GuardianCheckpointIntent::new(begin.checkpoint_id(), begin.boundary_id());
            let first_seed = checkpoint_catalog_issue_test_adoption_seed(
                guardian_incarnation,
                mux_incarnation,
                adoption_request_id,
                pane_id,
                generation,
                sequence,
                adoption_effect_id,
                intent,
            )?;
            let second_seed = checkpoint_catalog_issue_test_adoption_seed(
                guardian_incarnation,
                mux_incarnation,
                adoption_request_id,
                pane_id,
                generation,
                sequence,
                adoption_effect_id,
                intent,
            )?;
            let shape = CheckpointStageRequestShape::from_request(&seal)?;
            let scope = CheckpointCatalogScope::Pane { pane_id };
            let first_candidate_id =
                checkpoint_catalog_candidate_id(scope, 1, None, &shape, &first_seed);
            let second_candidate_id =
                checkpoint_catalog_candidate_id(scope, 1, None, &shape, &second_seed);
            assert_eq!(second_candidate_id, first_candidate_id);
            let identity = CheckpointCatalogIdentity {
                scope,
                generation: 1,
                candidate_id: first_candidate_id,
            };
            let mut first_candidate = checkpoint_catalog_candidate_from_sealed_stage(
                &store.inner,
                &seal,
                identity,
                None,
                &first_seed,
            )?;
            let first_base = checkpoint_catalog_encode_candidate_base(&mut first_candidate)?;
            first_candidate.adoption_evidence = Some(checkpoint_catalog_seal_adoption_evidence(
                &store.inner,
                &first_candidate,
                first_seed,
            )?);
            let first_encoded = checkpoint_catalog_encode_candidate(&mut first_candidate)?;

            let mut second_candidate = checkpoint_catalog_candidate_from_sealed_stage(
                &store.inner,
                &seal,
                identity,
                None,
                &second_seed,
            )?;
            let second_base = checkpoint_catalog_encode_candidate_base(&mut second_candidate)?;
            second_candidate.adoption_evidence = Some(checkpoint_catalog_seal_adoption_evidence(
                &store.inner,
                &second_candidate,
                second_seed,
            )?);
            let second_encoded = checkpoint_catalog_encode_candidate(&mut second_candidate)?;
            assert_eq!(second_base, first_base);
            assert_eq!(second_encoded, first_encoded);

            let evidence_record_bytes =
                usize::try_from(CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_RECORD_BYTES)?;
            let base_bytes = first_base.len();
            assert_eq!(
                first_encoded.len(),
                base_bytes
                    .checked_add(evidence_record_bytes)
                    .ok_or("protected candidate length overflow")?
            );
            let complete_header = base_bytes
                .checked_add(GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES)
                .ok_or("complete evidence header prefix overflow")?;
            let evidence_ciphertext_bytes = evidence_record_bytes
                .checked_sub(GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES)
                .ok_or("evidence record has no ciphertext")?;
            let prefix_bytes = match case {
                0 => base_bytes,
                1 => base_bytes
                    .checked_add(1)
                    .ok_or("first evidence prefix overflow")?,
                2 => base_bytes
                    .checked_add(GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES / 2)
                    .ok_or("middle evidence header prefix overflow")?,
                3 => complete_header,
                4 => complete_header
                    .checked_add(1)
                    .ok_or("first evidence ciphertext prefix overflow")?,
                5 => complete_header
                    .checked_add(evidence_ciphertext_bytes / 2)
                    .ok_or("middle evidence ciphertext prefix overflow")?,
                6 => first_encoded.len(),
                _ => unreachable!("bounded evidence crash case"),
            };
            let canonical = checkpoint_catalog_path(
                &store.inner,
                identity,
                CheckpointCatalogPathRole::Candidate,
            )?;
            let staging = checkpoint_catalog_staging_path(&store.inner, &canonical)?;
            let mut torn = create_private_file_new_at(
                &store.inner.directory,
                &store.inner.directory_path,
                &staging,
            )?;
            torn.write_all(&first_encoded[..prefix_bytes])?;
            torn.sync_all()?;
            store.inner.directory.sync_all()?;

            checkpoint_catalog_publish_test_adoption(
                &store,
                guardian_incarnation,
                mux_incarnation,
                adoption_request_id,
                pane_id,
                generation,
                sequence,
                adoption_effect_id,
                intent,
            )?;
            assert_eq!(std::fs::read(&canonical)?, second_encoded);
            assert!(matches!(
                std::fs::symlink_metadata(&staging),
                Err(error) if error.kind() == ErrorKind::NotFound
            ));

            let first_scan = checkpoint_catalog_scan(&store.inner, scope)?;
            assert_eq!(first_scan.published.len(), 1);
            assert!(first_scan.unpublished_candidates.is_empty());
            assert!(first_scan.staged_files.is_empty());
            assert_eq!(first_scan.relevant_files, 2);
            let published = &first_scan.published[0];
            assert_eq!(published.metadata.identity, identity);
            assert_eq!(published.candidate_path, canonical);
            let marker_before_retry = std::fs::read(&published.marker_path)?;

            checkpoint_catalog_publish_test_adoption(
                &store,
                guardian_incarnation,
                mux_incarnation,
                adoption_request_id,
                pane_id,
                generation,
                sequence,
                adoption_effect_id,
                intent,
            )?;
            let recovered = checkpoint_catalog_scan(&store.inner, scope)?;
            assert_eq!(recovered.published.len(), 1);
            assert!(recovered.unpublished_candidates.is_empty());
            assert!(recovered.staged_files.is_empty());
            assert_eq!(recovered.relevant_files, 2);
            assert_eq!(std::fs::read(&canonical)?, second_encoded);
            assert_eq!(
                std::fs::read(&recovered.published[0].marker_path)?,
                marker_before_retry
            );
        }
        Ok(())
    }

    #[test]
    fn checkpoint_catalog_publication_quarantines_non_prefix_staging_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-catalog-stage-corrupt-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let identity = CheckpointCatalogIdentity {
            scope: CheckpointCatalogScope::Pane {
                pane_id: Uuid::from_u128(0xc501),
            },
            generation: 1,
            candidate_id: Uuid::from_u128(0xc502),
        };
        let canonical =
            checkpoint_catalog_path(&store.inner, identity, CheckpointCatalogPathRole::Candidate)?;
        let staging = checkpoint_catalog_staging_path(&store.inner, &canonical)?;
        let mut corrupt = create_private_file_new_at(
            &store.inner.directory,
            &store.inner.directory_path,
            &staging,
        )?;
        corrupt.write_all(b"wrong-prefix")?;
        corrupt.sync_all()?;
        store.inner.directory.sync_all()?;

        assert!(matches!(
            checkpoint_catalog_publish_file(
                &store.inner,
                &canonical,
                b"right-prefix-and-complete-payload",
                "checkpoint-catalog-test-corrupt-write",
                "checkpoint-catalog-test-corrupt-sync",
            ),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        assert_eq!(std::fs::read(&staging)?, b"wrong-prefix");
        assert!(matches!(
            std::fs::symlink_metadata(&canonical),
            Err(error) if error.kind() == ErrorKind::NotFound
        ));
        Ok(())
    }

    #[test]
    fn checkpoint_catalog_publication_recovers_post_rename_pre_directory_sync_cut()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-catalog-post-rename-cut-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let identity = CheckpointCatalogIdentity {
            scope: CheckpointCatalogScope::Pane {
                pane_id: Uuid::from_u128(0xc551),
            },
            generation: 1,
            candidate_id: Uuid::from_u128(0xc552),
        };
        let canonical =
            checkpoint_catalog_path(&store.inner, identity, CheckpointCatalogPathRole::Candidate)?;
        let staging = checkpoint_catalog_staging_path(&store.inner, &canonical)?;
        let expected = b"synced-staging-before-atomic-rename";
        let mut file = create_private_file_new_at(
            &store.inner.directory,
            &store.inner.directory_path,
            &staging,
        )?;
        file.write_all(expected)?;
        file.sync_all()?;
        let staged_metadata = file.metadata()?;
        let staged_identity =
            FileIdentity::capture(&staged_metadata, Some(u64::try_from(expected.len())?));
        checkpoint_catalog_publish_noreplace(
            &store.inner.directory,
            output_child_name(&store.inner.directory_path, &staging)?,
            output_child_name(&store.inner.directory_path, &canonical)?,
        )?;

        let recovered = checkpoint_catalog_publish_file(
            &store.inner,
            &canonical,
            expected,
            "checkpoint-catalog-test-post-rename-write",
            "checkpoint-catalog-test-post-rename-sync",
        )?;
        assert_eq!(recovered, staged_identity);
        assert_eq!(std::fs::read(&canonical)?, expected);
        assert!(matches!(
            std::fs::symlink_metadata(&staging),
            Err(error) if error.kind() == ErrorKind::NotFound
        ));
        Ok(())
    }

    #[test]
    fn checkpoint_catalog_scan_rejects_and_retains_torn_canonical_candidate()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-catalog-canonical-torn-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let scope = CheckpointCatalogScope::Pane {
            pane_id: Uuid::from_u128(0xc601),
        };
        let identity = CheckpointCatalogIdentity {
            scope,
            generation: 1,
            candidate_id: Uuid::from_u128(0xc602),
        };
        let canonical =
            checkpoint_catalog_path(&store.inner, identity, CheckpointCatalogPathRole::Candidate)?;
        let torn = create_private_file_new_at(
            &store.inner.directory,
            &store.inner.directory_path,
            &canonical,
        )?;
        torn.sync_all()?;
        store.inner.directory.sync_all()?;

        assert!(matches!(
            checkpoint_catalog_scan(&store.inner, scope),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        assert_eq!(std::fs::symlink_metadata(&canonical)?.len(), 0);
        Ok(())
    }

    #[test]
    fn checkpoint_catalog_candidate_without_marker_is_ignored()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-catalog-candidate-cut-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let scope = CheckpointCatalogScope::Pane {
            pane_id: Uuid::from_u128(0xc3),
        };
        let identity = CheckpointCatalogIdentity {
            scope,
            generation: 1,
            candidate_id: Uuid::from_u128(0xc4),
        };
        let path =
            checkpoint_catalog_path(&store.inner, identity, CheckpointCatalogPathRole::Candidate)?;
        checkpoint_catalog_publish_file(
            &store.inner,
            &path,
            b"crash-before-candidate-file-sync-completed",
            "checkpoint-catalog-test-candidate-write",
            "checkpoint-catalog-test-candidate-sync",
        )?;
        store.inner.directory.sync_all()?;
        let scan = checkpoint_catalog_scan(&store.inner, scope)?;
        assert!(scan.published.is_empty());
        assert_eq!(scan.relevant_files, 1);
        assert_eq!(
            std::fs::read(path)?,
            b"crash-before-candidate-file-sync-completed"
        );
        Ok(())
    }

    #[test]
    fn checkpoint_catalog_marker_without_candidate_quarantines_exact_scope()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-catalog-marker-cut-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let scope = CheckpointCatalogScope::Genesis {
            spawn_effect_id: Uuid::from_u128(0xc5),
        };
        let identity = CheckpointCatalogIdentity {
            scope,
            generation: 1,
            candidate_id: Uuid::from_u128(0xc6),
        };
        let path =
            checkpoint_catalog_path(&store.inner, identity, CheckpointCatalogPathRole::Marker)?;
        let marker_value = checkpoint_catalog_test_marker(identity);
        let marker = checkpoint_catalog_encode_marker(&marker_value);
        checkpoint_catalog_publish_file(
            &store.inner,
            &path,
            &marker,
            "checkpoint-catalog-test-marker-write",
            "checkpoint-catalog-test-marker-sync",
        )?;
        store.inner.directory.sync_all()?;
        assert!(matches!(
            checkpoint_catalog_scan(&store.inner, scope),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        assert_eq!(std::fs::read(path)?, marker);
        Ok(())
    }

    #[test]
    fn checkpoint_catalog_marker_every_byte_is_checksum_bound() {
        let identity = CheckpointCatalogIdentity {
            scope: CheckpointCatalogScope::Pane {
                pane_id: Uuid::from_u128(0xc7),
            },
            generation: 1,
            candidate_id: Uuid::from_u128(0xc8),
        };
        let marker_value = checkpoint_catalog_test_marker(identity);
        let marker = checkpoint_catalog_encode_marker(&marker_value);
        assert_eq!(
            checkpoint_catalog_decode_marker(&marker).expect("decode canonical marker"),
            checkpoint_catalog_test_marker(identity)
        );
        for index in 0..marker.len() {
            let mut mutated = marker.clone();
            mutated[index] ^= 1;
            assert!(
                checkpoint_catalog_decode_marker(&mutated).is_err(),
                "byte {index}"
            );
        }
    }

    #[test]
    fn checkpoint_catalog_v2_pair_is_classified_preserved_and_never_authorizes_pane_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-catalog-legacy-v2-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let guardian_incarnation = Uuid::from_u128(0xc901);
        let mux_incarnation = Uuid::from_u128(0xc902);
        let pane_id = Uuid::from_u128(0xc903);
        let generation = 1;
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        let mut state = checkpoint_catalog_claimed_protocol_state(
            guardian_incarnation,
            mux_incarnation,
            pane_id,
        )?;
        let scope = CheckpointCatalogScope::Pane { pane_id };

        let (first_checkpoint_id, replay_semantics_id) = checkpoint_catalog_stage_and_publish(
            &pipeline,
            &store,
            &journal,
            &mut state,
            guardian_incarnation,
            mux_incarnation,
            pane_id,
            generation,
            1,
            0xcb00,
            b"one",
            b"one",
        )?;
        let protected = checkpoint_catalog_scan(&store.inner, scope)?;
        assert_eq!(protected.published.len(), 1);
        assert_eq!(
            protected.published[0].format,
            CheckpointCatalogFormat::ProtectedV3
        );
        let protected_first = protected.published[0].clone();
        let (legacy_candidate_bytes, legacy_marker_bytes) =
            checkpoint_catalog_rewrite_protected_member_as_legacy_fixture(
                &store.inner,
                &protected_first,
            )?;

        let legacy = checkpoint_catalog_scan(&store.inner, scope)?;
        assert_eq!(legacy.published.len(), 1);
        let legacy_first = &legacy.published[0];
        assert_eq!(legacy_first.format, CheckpointCatalogFormat::LegacyV2);
        assert!(!legacy_first.format.authorizes_scope(scope));
        assert_eq!(
            std::fs::read(&legacy_first.candidate_path)?,
            legacy_candidate_bytes
        );
        assert_eq!(
            std::fs::read(&legacy_first.marker_path)?,
            legacy_marker_bytes
        );
        let stage_scope = CheckpointStagePathScope::Pane {
            pane_id,
            generation,
        };
        assert_eq!(
            store.exact_catalog_member(stage_scope, first_checkpoint_id)?,
            None,
            "legacy metadata and SHA fields cannot synthesize Pane recovery authority"
        );
        assert_eq!(
            store.latest_compatible_catalog_member(stage_scope, replay_semantics_id)?,
            None,
            "legacy Pane entries remain readable but are never recovery-selectable"
        );

        let (second_checkpoint_id, second_replay_semantics_id) =
            checkpoint_catalog_stage_and_publish(
                &pipeline,
                &store,
                &journal,
                &mut state,
                guardian_incarnation,
                mux_incarnation,
                pane_id,
                generation,
                2,
                0xcc00,
                b"-two",
                b"one-two",
            )?;
        let upgraded = checkpoint_catalog_scan(&store.inner, scope)?;
        assert_eq!(upgraded.published.len(), 2);
        assert_eq!(
            upgraded.published[0].format,
            CheckpointCatalogFormat::LegacyV2
        );
        assert_eq!(
            upgraded.published[1].format,
            CheckpointCatalogFormat::ProtectedV3
        );
        assert_eq!(
            std::fs::read(&upgraded.published[0].candidate_path)?,
            legacy_candidate_bytes
        );
        assert_eq!(
            std::fs::read(&upgraded.published[0].marker_path)?,
            legacy_marker_bytes
        );
        assert_eq!(
            store.exact_catalog_member(stage_scope, first_checkpoint_id)?,
            None
        );
        assert_eq!(
            store.exact_catalog_member(stage_scope, second_checkpoint_id)?,
            Some(second_checkpoint_id)
        );
        assert_eq!(
            store.latest_compatible_catalog_member(stage_scope, second_replay_semantics_id)?,
            Some(second_checkpoint_id),
            "a protected v3 successor is recovery-selectable across a legacy predecessor"
        );

        checkpoint_catalog_stage_and_publish(
            &pipeline,
            &store,
            &journal,
            &mut state,
            guardian_incarnation,
            mux_incarnation,
            pane_id,
            generation,
            3,
            0xcd00,
            b"-three",
            b"one-two-three",
        )?;
        let before_downgrade = checkpoint_catalog_scan(&store.inner, scope)?;
        assert_eq!(before_downgrade.published.len(), 3);
        let protected_third = before_downgrade.published[2].clone();
        checkpoint_catalog_rewrite_protected_member_as_legacy_fixture(
            &store.inner,
            &protected_third,
        )?;
        let retained_bytes = before_downgrade
            .published
            .iter()
            .flat_map(|member| [&member.candidate_path, &member.marker_path])
            .map(|path| Ok((path.clone(), std::fs::read(path)?)))
            .collect::<Result<Vec<_>, std::io::Error>>()?;
        assert!(matches!(
            checkpoint_catalog_scan(&store.inner, scope),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        for (path, bytes) in retained_bytes {
            assert_eq!(
                std::fs::read(path)?,
                bytes,
                "a rejected v3-to-v2 downgrade must not mutate any catalog artifact"
            );
        }
        Ok(())
    }

    #[test]
    fn checkpoint_catalog_genesis_marker_rejects_nonzero_sequence_and_wrong_spawn() {
        let spawn_effect_id = Uuid::from_u128(0xe301);
        let identity = CheckpointCatalogIdentity {
            scope: CheckpointCatalogScope::Genesis { spawn_effect_id },
            generation: 1,
            candidate_id: Uuid::from_u128(0xe302),
        };
        let marker = checkpoint_catalog_test_marker(identity);
        let encoded = checkpoint_catalog_encode_marker(&marker);
        assert_eq!(
            checkpoint_catalog_decode_marker(&encoded).expect("canonical Genesis marker"),
            marker
        );

        let mut nonzero_sequence = marker;
        nonzero_sequence.adoption_sequence = 1;
        assert!(
            checkpoint_catalog_decode_marker(&checkpoint_catalog_encode_marker(&nonzero_sequence))
                .is_err()
        );
        let mut wrong_spawn = marker;
        wrong_spawn.adoption_effect_id = Uuid::from_u128(0xe303);
        assert!(
            checkpoint_catalog_decode_marker(&checkpoint_catalog_encode_marker(&wrong_spawn))
                .is_err()
        );
        let mut second_generation = marker;
        second_generation.identity.generation = 2;
        assert!(
            checkpoint_catalog_decode_marker(&checkpoint_catalog_encode_marker(&second_generation))
                .is_err()
        );
    }

    fn checkpoint_catalog_test_unpublished_candidate(
        identity: CheckpointCatalogIdentity,
        path: PathBuf,
        inode: u64,
    ) -> DiscoveredCheckpointCatalogCandidate {
        DiscoveredCheckpointCatalogCandidate {
            identity,
            path,
            file_identity: FileIdentity {
                device: 1,
                inode,
                mode: 0o100600,
                owner: geteuid().as_raw(),
                links: 1,
                expected_len: Some(100),
            },
            bytes: 100,
            published: false,
        }
    }

    #[test]
    fn checkpoint_catalog_genesis_candidate_retry_reuses_one_identity_and_adds_only_marker() {
        let reservation = checkpoint_catalog_test_genesis_reservation_binding();
        let identity = CheckpointCatalogIdentity {
            scope: CheckpointCatalogScope::Genesis {
                spawn_effect_id: reservation.spawn_effect_id,
            },
            generation: 1,
            candidate_id: checkpoint_catalog_genesis_candidate_id(reservation),
        };
        let candidate_path = PathBuf::from("deterministic-genesis-candidate");
        let empty = CheckpointCatalogScan {
            published: Vec::new(),
            unpublished_candidates: Vec::new(),
            staged_files: Vec::new(),
            relevant_files: 0,
            relevant_bytes: 0,
        };
        let create = checkpoint_catalog_genesis_candidate_plan(&empty, identity, &candidate_path)
            .expect("empty scope creates its deterministic candidate");
        assert!(matches!(
            create,
            CheckpointCatalogGenesisCandidatePlan::Create
        ));
        assert_eq!(
            checkpoint_catalog_genesis_added_resources(&create, None, 1_000, 344)
                .expect("fresh resource delta"),
            (2, 1_344)
        );

        let retry = CheckpointCatalogScan {
            published: Vec::new(),
            unpublished_candidates: vec![checkpoint_catalog_test_unpublished_candidate(
                identity,
                candidate_path.clone(),
                7,
            )],
            staged_files: Vec::new(),
            relevant_files: 1,
            relevant_bytes: 1_000,
        };
        let reuse = checkpoint_catalog_genesis_candidate_plan(&retry, identity, &candidate_path)
            .expect("exact candidate-only retry is reusable");
        assert!(matches!(
            reuse,
            CheckpointCatalogGenesisCandidatePlan::Reuse(_)
        ));
        assert_eq!(
            checkpoint_catalog_genesis_added_resources(&reuse, None, 1_000, 344)
                .expect("candidate-only resource delta"),
            (1, 344),
            "a candidate-only retry may add only the missing marker"
        );
        checkpoint_catalog_require_exact_genesis_candidate_bytes(b"exact", b"exact")
            .expect("exact immutable candidate bytes");
        assert!(
            checkpoint_catalog_require_exact_genesis_candidate_bytes(b"corrupt", b"exact").is_err(),
            "candidate corruption must fail closed before marker creation"
        );

        let wrong_identity = CheckpointCatalogIdentity {
            candidate_id: Uuid::from_u128(0xe401),
            ..identity
        };
        let fork = CheckpointCatalogScan {
            published: Vec::new(),
            unpublished_candidates: vec![checkpoint_catalog_test_unpublished_candidate(
                wrong_identity,
                PathBuf::from("fork-candidate"),
                8,
            )],
            staged_files: Vec::new(),
            relevant_files: 1,
            relevant_bytes: 100,
        };
        assert!(
            checkpoint_catalog_genesis_candidate_plan(&fork, identity, &candidate_path).is_err(),
            "a reservation scope cannot allocate around a candidate fork"
        );
        let duplicate = CheckpointCatalogScan {
            published: Vec::new(),
            unpublished_candidates: vec![
                checkpoint_catalog_test_unpublished_candidate(identity, candidate_path.clone(), 9),
                checkpoint_catalog_test_unpublished_candidate(
                    wrong_identity,
                    PathBuf::from("second-candidate"),
                    10,
                ),
            ],
            staged_files: Vec::new(),
            relevant_files: 2,
            relevant_bytes: 200,
        };
        assert!(
            checkpoint_catalog_genesis_candidate_plan(&duplicate, identity, &candidate_path)
                .is_err(),
            "multiple candidate-only files are a poisoned fork, not a retry surface"
        );
    }

    #[test]
    fn checkpoint_catalog_genesis_scope_cannot_publish_a_second_member() {
        let reservation = checkpoint_catalog_test_genesis_reservation_binding();
        let metadata = checkpoint_catalog_test_genesis_metadata(reservation);
        let file_identity = FileIdentity {
            device: 1,
            inode: 1,
            mode: 0o100600,
            owner: geteuid().as_raw(),
            links: 1,
            expected_len: Some(1),
        };
        let first = PublishedCheckpointCatalogMember {
            format: CheckpointCatalogFormat::ProtectedV3,
            metadata,
            candidate_checksum: [0x71; 32],
            candidate_path: PathBuf::from("genesis-candidate-one"),
            candidate_file_identity: file_identity,
            marker_path: PathBuf::from("genesis-marker-one"),
            marker_file_identity: file_identity,
        };
        let mut second = first.clone();
        second.metadata.identity.candidate_id = Uuid::from_u128(0xe501);
        second.metadata.checkpoint_id = [0x72; 32];
        second.metadata.boundary_id = [0x73; 32];
        second.candidate_checksum = [0x74; 32];
        second.candidate_path = PathBuf::from("genesis-candidate-two");
        second.marker_path = PathBuf::from("genesis-marker-two");
        assert!(checkpoint_catalog_validate_metadata(&second.metadata).is_ok());
        assert!(
            checkpoint_catalog_validate_chain(&mut vec![first, second]).is_err(),
            "one retained Spawn effect cannot gain a second published Genesis member"
        );
    }

    #[test]
    fn published_genesis_admission_permit_is_not_cloneable() {
        trait AmbiguousIfClone<Marker> {
            fn probe() {}
        }
        impl<T: ?Sized> AmbiguousIfClone<()> for T {}
        struct ImplementsClone;
        impl<T: Clone> AmbiguousIfClone<ImplementsClone> for T {}

        let _ = <GuardianPublishedGenesisAdmissionPermitV1 as AmbiguousIfClone<_>>::probe;
    }

    fn checkpoint_catalog_test_member(
        scope: CheckpointCatalogScope,
        generation: u64,
        predecessor: Option<&PublishedCheckpointCatalogMember>,
        identity_byte: u8,
    ) -> PublishedCheckpointCatalogMember {
        let candidate_id = Uuid::from_u128(u128::from(identity_byte) + 1);
        let candidate_checksum = [identity_byte.wrapping_add(0x40); 32];
        let predecessor = predecessor.map(|prior| CheckpointCatalogPredecessor {
            generation: prior.metadata.identity.generation,
            candidate_id: prior.metadata.identity.candidate_id,
            candidate_checksum: prior.candidate_checksum,
            checkpoint_id: prior.metadata.checkpoint_id,
            boundary_id: prior.metadata.boundary_id,
        });
        let file_identity = FileIdentity {
            device: 1,
            inode: u64::from(identity_byte) + 1,
            mode: 0o100600,
            owner: geteuid().as_raw(),
            links: 1,
            expected_len: Some(1),
        };
        PublishedCheckpointCatalogMember {
            format: CheckpointCatalogFormat::ProtectedV3,
            metadata: CheckpointCatalogMetadata {
                identity: CheckpointCatalogIdentity {
                    scope,
                    generation,
                    candidate_id,
                },
                predecessor,
                upload_id: Uuid::from_u128(u128::from(identity_byte) + 0x100),
                completion_id: Uuid::from_u128(u128::from(identity_byte) + 0x200),
                checkpoint_id: [identity_byte.wrapping_add(1); 32],
                boundary_id: [identity_byte.wrapping_add(0x20); 32],
                terminal_payload_digest: [identity_byte.wrapping_add(0x30); 32],
                total_bytes: 1,
                chunk_count: 1,
                capture_generation: 1,
                replay_semantics_id: [0x55; 32],
                rows: 24,
                cols: 80,
                adoption_mux_incarnation: Uuid::from_u128(0x300),
                adoption_effect_id: Uuid::from_u128(u128::from(identity_byte) + 0x400),
                adoption_sequence: generation,
                genesis_durable_pane_id: Uuid::nil(),
                genesis_origin_request_id: Uuid::nil(),
                genesis_spawn_payload_bytes: 0,
                genesis_spawn_payload_digest: [0; 32],
                genesis_spawning_mux_build_identity_digest: [0; 32],
                genesis_live_guardian_build_identity_digest: [0; 32],
                genesis_pixel_width: 0,
                genesis_pixel_height: 0,
            },
            candidate_checksum,
            candidate_path: PathBuf::from(format!("candidate-{identity_byte}")),
            candidate_file_identity: file_identity,
            marker_path: PathBuf::from(format!("marker-{identity_byte}")),
            marker_file_identity: file_identity,
        }
    }

    #[test]
    fn checkpoint_catalog_chain_rejects_gap_fork_duplicate_and_mixed_scope() {
        let scope = CheckpointCatalogScope::Pane {
            pane_id: Uuid::from_u128(0xd1),
        };
        let first = checkpoint_catalog_test_member(scope, 1, None, 1);
        let second = checkpoint_catalog_test_member(scope, 2, Some(&first), 2);
        let third = checkpoint_catalog_test_member(scope, 3, Some(&second), 3);
        let mut valid_out_of_enumeration_order = vec![third.clone(), first.clone(), second.clone()];
        checkpoint_catalog_validate_chain(&mut valid_out_of_enumeration_order)
            .expect("generation and predecessor, never mtime, define order");
        assert_eq!(
            valid_out_of_enumeration_order
                .iter()
                .map(|member| member.metadata.identity.generation)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        let mut gap = vec![first.clone(), third.clone()];
        assert!(checkpoint_catalog_validate_chain(&mut gap).is_err());

        let fork = checkpoint_catalog_test_member(scope, 2, Some(&first), 4);
        let mut duplicate_generation = vec![first.clone(), second.clone(), fork];
        assert!(checkpoint_catalog_validate_chain(&mut duplicate_generation).is_err());

        let mut bad_predecessor = second.clone();
        bad_predecessor
            .metadata
            .predecessor
            .as_mut()
            .expect("second member predecessor")
            .candidate_checksum[0] ^= 1;
        let mut predecessor_mismatch = vec![first.clone(), bad_predecessor];
        assert!(checkpoint_catalog_validate_chain(&mut predecessor_mismatch).is_err());

        let genesis_scope = CheckpointCatalogScope::Genesis {
            spawn_effect_id: Uuid::from_u128(0xd2),
        };
        let mixed = checkpoint_catalog_test_member(genesis_scope, 2, Some(&first), 5);
        let mut mixed_scope = vec![first.clone(), mixed];
        assert!(checkpoint_catalog_validate_chain(&mut mixed_scope).is_err());

        let mut divergent_boundary = second.clone();
        divergent_boundary.metadata.boundary_id = first.metadata.boundary_id;
        let mut boundary_splice = vec![first.clone(), divergent_boundary];
        assert!(checkpoint_catalog_validate_chain(&mut boundary_splice).is_err());

        let mut duplicate_checkpoint = second;
        duplicate_checkpoint.metadata.checkpoint_id = first.metadata.checkpoint_id;
        let mut checkpoint_replay = vec![first, duplicate_checkpoint];
        assert!(checkpoint_catalog_validate_chain(&mut checkpoint_replay).is_err());
    }

    #[test]
    fn checkpoint_stage_resource_caps_match_the_protocol_envelope() {
        let policy = GuardianCheckpointStagePolicy::production()
            .validate()
            .expect("production checkpoint staging policy");
        let record_overhead = CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES;
        let chunks = u64::from(GUARDIAN_MAX_CHECKPOINT_CHUNKS);
        let expected_upload_bytes = GUARDIAN_MAX_CHECKPOINT_BYTES
            .checked_add(chunks.checked_mul(record_overhead).expect("chunk overhead"))
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES)
                        .expect("candidate size")
                        + record_overhead,
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(CHECKPOINT_STAGE_SEAL_PLAINTEXT_BYTES).expect("seal size")
                        + record_overhead,
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(CHECKPOINT_STAGE_ACK_PLAINTEXT_BYTES).expect("ack size")
                        + record_overhead,
                )
            })
            .expect("upload envelope");
        assert_eq!(CHECKPOINT_STAGE_MAX_FILES_PER_UPLOAD, 1_027);
        assert_eq!(CHECKPOINT_STAGE_MAX_BYTES_PER_UPLOAD, expected_upload_bytes);
        assert_eq!(expected_upload_bytes, 268_740_576);
        assert_eq!(
            policy.max_stage_files,
            policy.max_retained_uploads * CHECKPOINT_STAGE_MAX_FILES_PER_UPLOAD
        );
        assert_eq!(
            policy.max_stage_bytes,
            u64::try_from(policy.max_retained_uploads).expect("retention count")
                * CHECKPOINT_STAGE_MAX_BYTES_PER_UPLOAD
        );
        let expected_catalog_candidate_bytes = GUARDIAN_MAX_CHECKPOINT_BYTES
            + (u64::from(GUARDIAN_MAX_CHECKPOINT_CHUNKS) + 2) * record_overhead
            + u64::try_from(CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES).expect("candidate bytes")
            + u64::try_from(CHECKPOINT_STAGE_SEAL_PLAINTEXT_BYTES).expect("seal bytes")
            + u64::try_from(CHECKPOINT_CATALOG_HEADER_BYTES).expect("catalog header bytes")
            + u64::try_from(OUTPUT_MANIFEST_CHECKSUM_BYTES).expect("catalog checksum bytes")
            + CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_RECORD_BYTES;
        assert_eq!(
            CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_RECORD_BYTES,
            u64::from(GUARDIAN_CHECKPOINT_CATALOG_ADOPTION_EVIDENCE_BYTES)
                + CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES
        );
        assert!(include_str!("output.rs").contains(".try_reserve_exact(evidence_bytes)"));
        assert_eq!(
            CHECKPOINT_CATALOG_MAX_CANDIDATE_BYTES,
            expected_catalog_candidate_bytes
        );
        assert_eq!(CHECKPOINT_CATALOG_MAX_PUBLISHED_MEMBERS, 8);
        assert_eq!(CHECKPOINT_CATALOG_MAX_RELEVANT_FILES, 24);
        assert_eq!(
            CHECKPOINT_CATALOG_MAX_RELEVANT_BYTES,
            expected_catalog_candidate_bytes * 16
                + u64::try_from(CHECKPOINT_CATALOG_MARKER_BYTES).expect("marker bytes") * 8
        );
    }

    #[test]
    fn checkpoint_stage_shared_logical_identities_are_content_stable_and_complete_only()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = include_str!("output.rs");
        assert!(source.contains(concat!(
            "fn zeroizing_test_bytes(bytes: &[u8]) -> ",
            "Zeroizing<Vec<u8>>"
        )));
        for forbidden in [
            concat!("Zeroizing::new(chunk", ".to_vec())"),
            concat!(
                "let mut wire = vec![0_u8; ",
                "CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES"
            ),
            concat!(
                "let mut wire = vec![\n            0_u8;\n            ",
                "CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES"
            ),
        ] {
            assert!(!source.contains(forbidden));
        }

        let spawn_effect_id = Uuid::from_u128(0x31);
        let upload_id = Uuid::from_u128(0x32);
        let payload = b"manifest-layout-fixture";
        let chunk_bytes = 8;
        let begin = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            None,
        )?;
        let shape = CheckpointStageRequestShape::from_request(&begin)?;
        let begin_payload: Zeroizing<Vec<u8>> = shape.begin_payload()?;
        let candidate_identity =
            GuardianCheckpointCandidateIdentityV1::from_canonical_begin_plaintext(&begin_payload)?;
        let exact_retry_identity =
            GuardianCheckpointCandidateIdentityV1::from_canonical_begin_plaintext(&begin_payload)?;
        assert_eq!(candidate_identity, exact_retry_identity);

        let changed_begin = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            Uuid::from_u128(0x33),
            payload,
            chunk_bytes,
            None,
        )?;
        let changed_shape = CheckpointStageRequestShape::from_request(&changed_begin)?;
        let changed_begin_payload: Zeroizing<Vec<u8>> = changed_shape.begin_payload()?;
        let changed_candidate_identity =
            GuardianCheckpointCandidateIdentityV1::from_canonical_begin_plaintext(
                &changed_begin_payload,
            )?;
        assert_ne!(candidate_identity, changed_candidate_identity);

        let logical_chunk_set = |plaintext: &[u8]| -> Result<
            GuardianCheckpointOrderedChunkSetIdentityV1,
            GuardianCheckpointCipherError,
        > {
            assert_eq!(
                plaintext.len(),
                usize::try_from(shape.total_bytes).expect("test payload length")
            );
            let mut builder = GuardianCheckpointOrderedChunkSetBuilderV1::new(
                shape.total_bytes,
                shape.chunk_bytes,
                shape.total_chunks,
            )?;
            for (index, chunk) in plaintext
                .chunks(usize::try_from(shape.chunk_bytes).expect("test chunk size"))
                .enumerate()
            {
                let index = u32::try_from(index).expect("test chunk index");
                let offset = u64::from(index) * u64::from(shape.chunk_bytes);
                let chunk: Zeroizing<Vec<u8>> = zeroizing_test_bytes(chunk);
                builder.push_authenticated_chunk(index, offset, &chunk)?;
            }
            builder.finish()
        };
        let chunk_set_identity = logical_chunk_set(payload)?;
        let exact_retry_chunk_set_identity = logical_chunk_set(payload)?;
        assert_eq!(chunk_set_identity, exact_retry_chunk_set_identity);

        let mut changed_payload: Zeroizing<Vec<u8>> = zeroizing_test_bytes(payload);
        changed_payload[0] ^= 1;
        let changed_chunk_set_identity = logical_chunk_set(changed_payload.as_slice())?;
        assert_ne!(chunk_set_identity, changed_chunk_set_identity);

        let mut incomplete = GuardianCheckpointOrderedChunkSetBuilderV1::new(
            shape.total_bytes,
            shape.chunk_bytes,
            shape.total_chunks,
        )?;
        let first_chunk = zeroizing_test_bytes(
            &payload[..usize::try_from(shape.chunk_bytes).expect("test first chunk")],
        );
        incomplete.push_authenticated_chunk(0, 0, &first_chunk)?;
        assert!(incomplete.finish().is_err());
        Ok(())
    }

    #[test]
    fn checkpoint_stage_begin_chunk_retry_and_gap_are_durable_and_exact()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, poll, pipeline) = pipeline_with_policy(
            "ft-guardian-checkpoint-stage-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let spawn_effect_id = Uuid::from_u128(0x41);
        let upload_id = Uuid::from_u128(0x42);
        let payload = b"durable-checkpoint-fragments";
        let chunk_bytes = 8;
        let chunk_bytes_usize = usize::try_from(chunk_bytes)?;
        let begin = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            None,
        )?;
        let absent_query = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Query,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            None,
        )?;
        assert_eq!(
            store.apply_query(absent_query)?,
            GuardianCheckpointStageReplyV1::Absent { upload_id }
        );
        assert_eq!(
            store.apply_begin(&begin)?,
            GuardianCheckpointStageReplyV1::Ready {
                upload_id,
                next_index: 0,
                committed_bytes: 0,
            }
        );
        let ready_query = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Query,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            None,
        )?;
        assert_eq!(
            store.apply_query(ready_query)?,
            GuardianCheckpointStageReplyV1::Ready {
                upload_id,
                next_index: 0,
                committed_bytes: 0,
            }
        );

        let second = &payload[8..16];
        let out_of_order = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Chunk,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            Some((1, second)),
        )?;
        assert!(matches!(
            store.apply_chunk(out_of_order),
            Err(GuardianCheckpointStageStoreError::OutOfOrder)
        ));

        for (index, bytes) in payload.chunks(chunk_bytes_usize).enumerate() {
            let index = u32::try_from(index)?;
            let chunk = checkpoint_test_genesis_request(
                GuardianCheckpointStageKindV1::Chunk,
                spawn_effect_id,
                upload_id,
                payload,
                chunk_bytes,
                Some((index, bytes)),
            )?;
            let expected_bytes = u64::from(index + 1)
                .checked_mul(u64::from(chunk_bytes))
                .ok_or("fixture progress overflow")?
                .min(u64::try_from(payload.len())?);
            assert_eq!(
                store.apply_chunk(chunk)?,
                GuardianCheckpointStageReplyV1::Progress {
                    upload_id,
                    next_index: index + 1,
                    committed_bytes: expected_bytes,
                }
            );
        }
        drop(store);
        drop(pipeline);
        drop(poll);
        let (_reopened_poll, reopened_pipeline) =
            reopen_pipeline(&directory, OutputSegmentPolicy::production())?;
        let store = reopened_pipeline.checkpoint_stage_store();
        let total_chunks = u32::try_from(payload.chunks(chunk_bytes_usize).count())?;
        let first_retry = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Chunk,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            Some((0, &payload[..8])),
        )?;
        assert_eq!(
            store.apply_chunk(first_retry)?,
            GuardianCheckpointStageReplyV1::Progress {
                upload_id,
                next_index: 1,
                committed_bytes: 8,
            }
        );
        let expected_durable_records = usize::try_from(total_chunks)?
            .checked_add(1)
            .ok_or("fixture durable-record count overflow")?;
        assert_eq!(
            store
                .inner
                .durable_records
                .lock()
                .expect("checkpoint durability cache")
                .len(),
            expected_durable_records
        );
        let conflicting_first_retry = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Chunk,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            Some((0, b"conflict")),
        )?;
        assert!(matches!(
            store.apply_chunk(conflicting_first_retry),
            Err(GuardianCheckpointStageStoreError::Conflict)
        ));
        assert_eq!(
            store.apply_begin(&begin)?,
            GuardianCheckpointStageReplyV1::Ready {
                upload_id,
                next_index: total_chunks,
                committed_bytes: u64::try_from(payload.len())?,
            }
        );
        let completed_upload_query = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Query,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            None,
        )?;
        assert_eq!(
            store.apply_query(completed_upload_query)?,
            GuardianCheckpointStageReplyV1::Progress {
                upload_id,
                next_index: total_chunks,
                committed_bytes: u64::try_from(payload.len())?,
            }
        );
        Ok(())
    }

    #[test]
    fn checkpoint_stage_torn_candidate_poison_is_retained_but_fresh_upload_progresses()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-checkpoint-cut-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let spawn_effect_id = Uuid::from_u128(0x51);
        let upload_id = Uuid::from_u128(0x52);
        let payload = b"crash-cut-checkpoint";
        let begin = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            upload_id,
            payload,
            8,
            None,
        )?;
        let shape = CheckpointStageRequestShape::from_request(&begin)?;
        let torn_path = checkpoint_candidate_path(&store.inner, shape.key())?;
        let mut torn = create_private_file_new_at(
            &store.inner.directory,
            &store.inner.directory_path,
            &torn_path,
        )?;
        torn.write_all(b"FTGC")?;
        torn.sync_all()?;
        store.inner.directory.sync_all()?;
        drop(torn);
        assert!(matches!(
            store.apply_begin(&begin),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        assert_eq!(std::fs::metadata(&torn_path)?.len(), 4);

        let fresh_upload_id = Uuid::from_u128(0x53);
        let fresh = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            fresh_upload_id,
            payload,
            8,
            None,
        )?;
        assert_eq!(
            store.apply_begin(&fresh)?,
            GuardianCheckpointStageReplyV1::Ready {
                upload_id: fresh_upload_id,
                next_index: 0,
                committed_bytes: 0,
            }
        );
        assert_eq!(std::fs::metadata(&torn_path)?.len(), 4);
        Ok(())
    }

    #[test]
    fn checkpoint_stage_conflicting_begin_cannot_relabel_the_durable_candidate()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-checkpoint-conflict-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let spawn_effect_id = Uuid::from_u128(0x61);
        let upload_id = Uuid::from_u128(0x62);
        let original = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            upload_id,
            b"original-terminal-state",
            8,
            None,
        )?;
        let conflicting = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            upload_id,
            b"different-terminal-state",
            8,
            None,
        )?;
        let expected = GuardianCheckpointStageReplyV1::Ready {
            upload_id,
            next_index: 0,
            committed_bytes: 0,
        };
        assert_eq!(store.apply_begin(&original)?, expected);
        assert!(matches!(
            store.apply_begin(&conflicting),
            Err(GuardianCheckpointStageStoreError::Conflict)
        ));
        assert_eq!(store.apply_begin(&original)?, expected);
        Ok(())
    }

    #[test]
    fn checkpoint_stage_global_retention_cap_fails_closed_without_reclamation()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-checkpoint-retention-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let payload = b"bounded-retention-fixture";
        for ordinal in 0..CHECKPOINT_STAGE_MAX_RETAINED_UPLOADS {
            let ordinal = u128::try_from(ordinal)?;
            let begin = checkpoint_test_genesis_request(
                GuardianCheckpointStageKindV1::Begin,
                Uuid::from_u128(0x700 + ordinal),
                Uuid::from_u128(0x800 + ordinal),
                payload,
                8,
                None,
            )?;
            assert!(matches!(
                store.apply_begin(&begin)?,
                GuardianCheckpointStageReplyV1::Ready {
                    next_index: 0,
                    committed_bytes: 0,
                    ..
                }
            ));
        }
        let refused = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            Uuid::from_u128(0x900),
            Uuid::from_u128(0x901),
            payload,
            8,
            None,
        )?;
        assert!(matches!(
            store.apply_begin(&refused),
            Err(GuardianCheckpointStageStoreError::Capacity)
        ));
        let census = checkpoint_stage_census(&store.inner)?;
        assert_eq!(census.uploads.len(), CHECKPOINT_STAGE_MAX_RETAINED_UPLOADS);
        assert_eq!(census.total_files, CHECKPOINT_STAGE_MAX_RETAINED_UPLOADS);
        Ok(())
    }

    #[test]
    fn checkpoint_stage_malformed_prefixed_entry_is_never_ignored()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-checkpoint-raw-name-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        // The byte parser has a separate invalid-UTF-8 negative control. Use a
        // representable but malformed prefixed name for the filesystem proof
        // because APFS rejects non-UTF-8 path components before the store can
        // inspect them.
        let path = store.inner.directory_path.join("checkpoint-invalid-name");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(b"retained")?;
        file.sync_all()?;
        store.inner.directory.sync_all()?;
        let begin = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            Uuid::from_u128(0xa01),
            Uuid::from_u128(0xa02),
            b"raw-name-census",
            8,
            None,
        )?;
        assert!(matches!(
            store.apply_begin(&begin),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        assert_eq!(std::fs::metadata(path)?.len(), 8);
        Ok(())
    }

    #[test]
    fn checkpoint_stage_torn_seal_is_quarantined_without_hiding_other_uploads()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, poll, pipeline) = pipeline_with_policy(
            "ft-guardian-checkpoint-seal-cut-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let spawn_effect_id = Uuid::from_u128(0xb01);
        let upload_id = Uuid::from_u128(0xb02);
        let payload = b"torn-seal-fixture";
        let chunk_bytes = 8_u32;
        let begin = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            None,
        )?;
        store.apply_begin(&begin)?;
        for (index, bytes) in payload.chunks(usize::try_from(chunk_bytes)?).enumerate() {
            store.apply_chunk(checkpoint_test_genesis_request(
                GuardianCheckpointStageKindV1::Chunk,
                spawn_effect_id,
                upload_id,
                payload,
                chunk_bytes,
                Some((u32::try_from(index)?, bytes)),
            )?)?;
        }
        let shape = CheckpointStageRequestShape::from_request(&begin)?;
        let census = checkpoint_stage_census(&store.inner)?;
        let inspection = checkpoint_inspect_upload(
            &store.inner,
            &census,
            &shape,
            CheckpointStageSealInspection::Reject,
        )?
        .ok_or("durable candidate disappeared")?;
        let seal_path = checkpoint_seal_path(&store.inner, shape.key(), inspection.publication_id)?;
        let mut torn = create_private_file_new_at(
            &store.inner.directory,
            &store.inner.directory_path,
            &seal_path,
        )?;
        torn.write_all(b"FTGC")?;
        torn.sync_all()?;
        store.inner.directory.sync_all()?;
        drop(torn);
        drop(store);
        drop(pipeline);
        drop(poll);

        let (_reopened_poll, reopened_pipeline) =
            reopen_pipeline(&directory, OutputSegmentPolicy::production())?;
        let store = reopened_pipeline.checkpoint_stage_store();
        let historical_retry = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Chunk,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            Some((0, &payload[..usize::try_from(chunk_bytes)?])),
        )?;
        assert_eq!(
            store.apply_chunk(historical_retry)?,
            GuardianCheckpointStageReplyV1::Progress {
                upload_id,
                next_index: 1,
                committed_bytes: u64::from(chunk_bytes),
            }
        );
        let query = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Query,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            None,
        )?;
        assert_eq!(
            store.apply_query(query)?,
            GuardianCheckpointStageReplyV1::Quarantined { upload_id }
        );
        assert!(matches!(
            store.apply_begin(&begin),
            Err(GuardianCheckpointStageStoreError::Poisoned)
        ));
        assert_eq!(std::fs::metadata(&seal_path)?.len(), 4);

        let fresh_upload_id = Uuid::from_u128(0xb03);
        let fresh = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            fresh_upload_id,
            payload,
            chunk_bytes,
            None,
        )?;
        assert!(matches!(
            store.apply_begin(&fresh)?,
            GuardianCheckpointStageReplyV1::Ready {
                upload_id,
                next_index: 0,
                committed_bytes: 0,
            } if upload_id == fresh_upload_id
        ));
        assert_eq!(std::fs::metadata(seal_path)?.len(), 4);
        Ok(())
    }

    #[test]
    fn checkpoint_stage_finalizers_require_seal_and_ack_expiry_are_exclusive()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-checkpoint-finalizer-conflict-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let spawn_effect_id = Uuid::from_u128(0xc01);
        let upload_id = Uuid::from_u128(0xc02);
        let payload = b"finalizer-conflict-fixture";
        let chunk_bytes = 8_u32;
        let begin = checkpoint_test_genesis_request(
            GuardianCheckpointStageKindV1::Begin,
            spawn_effect_id,
            upload_id,
            payload,
            chunk_bytes,
            None,
        )?;
        store.apply_begin(&begin)?;
        let shape = CheckpointStageRequestShape::from_request(&begin)?;
        let census = checkpoint_stage_census(&store.inner)?;
        let inspection = checkpoint_inspect_upload(
            &store.inner,
            &census,
            &shape,
            CheckpointStageSealInspection::Reject,
        )?
        .ok_or("durable candidate disappeared")?;
        let publication_id = inspection.publication_id;
        let ack_path = checkpoint_ack_path(&store.inner, shape.key(), publication_id)?;
        let expired_path = checkpoint_expiry_path(&store.inner, shape.key(), publication_id)?;
        let seal_path = checkpoint_seal_path(&store.inner, shape.key(), publication_id)?;
        let query = || {
            checkpoint_test_genesis_request(
                GuardianCheckpointStageKindV1::Query,
                spawn_effect_id,
                upload_id,
                payload,
                chunk_bytes,
                None,
            )
        };

        let mut ack = create_private_file_new_at(
            &store.inner.directory,
            &store.inner.directory_path,
            &ack_path,
        )?;
        ack.write_all(b"FTGC")?;
        ack.sync_all()?;
        store.inner.directory.sync_all()?;
        drop(ack);
        assert_eq!(
            store.apply_query(query()?)?,
            GuardianCheckpointStageReplyV1::Quarantined { upload_id }
        );

        for path in [&seal_path, &expired_path] {
            let mut file = create_private_file_new_at(
                &store.inner.directory,
                &store.inner.directory_path,
                path,
            )?;
            file.write_all(b"FTGC")?;
            file.sync_all()?;
        }
        store.inner.directory.sync_all()?;
        assert_eq!(
            store.apply_query(query()?)?,
            GuardianCheckpointStageReplyV1::Quarantined { upload_id }
        );
        for path in [ack_path, seal_path, expired_path] {
            assert_eq!(std::fs::metadata(path)?.len(), 4);
        }
        Ok(())
    }

    #[test]
    fn input_wal_reopen_is_withheld_without_anti_rollback_authority_and_stays_out_of_output_namespace()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-input-spawn-retry-",
            OutputSegmentPolicy::production(),
        )?;
        let guardian_incarnation = Uuid::from_u128(0x71);
        let pane_id = Uuid::from_u128(0x72);
        let path = input_journal_path(&pipeline.directory_path, guardian_incarnation, pane_id);
        assert!(
            !path
                .file_name()
                .and_then(OsStr::to_str)
                .expect("input journal file name")
                .starts_with(&pane_file_prefix(guardian_incarnation, pane_id))
        );

        let first = pipeline.prepare_input(guardian_incarnation, pane_id)?;
        drop(first);
        assert!(
            pipeline
                .prepare_input(guardian_incarnation, pane_id)
                .is_err(),
            "even a header-only reopen must be withheld without anti-rollback authority"
        );
        Ok(())
    }

    #[cfg(any(
        target_os = "android",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    #[test]
    fn output_directory_creation_stays_with_pinned_parent_across_aba_restore()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-parent-aba-")?;
        let pinned = open_directory_no_follow(&directory)?;
        let retained_original = directory.with_file_name(format!(
            "ft-guardian-output-parent-aba-original-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&directory, &retained_original)?;
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;

        create_private_directory_at(&pinned, OsStr::new(OUTPUT_DIRECTORY_NAME))?;
        let created = open_private_directory_at(&pinned, OsStr::new(OUTPUT_DIRECTORY_NAME))?;
        validate_private_directory_metadata(&created.metadata()?)?;
        pinned.sync_all()?;

        let retained_replacement = directory.with_file_name(format!(
            "ft-guardian-output-parent-aba-replacement-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&directory, &retained_replacement)?;
        std::fs::rename(&retained_original, &directory)?;

        assert!(directory.join(OUTPUT_DIRECTORY_NAME).is_dir());
        assert!(!retained_replacement.join(OUTPUT_DIRECTORY_NAME).exists());
        Ok(())
    }

    #[cfg(any(
        target_os = "android",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    #[test]
    fn output_artifact_access_stays_with_pinned_directory_across_aba_restore()
    -> Result<(), Box<dyn std::error::Error>> {
        let parent = kept_private_directory("ft-guardian-output-artifact-aba-")?;
        let output_directory = parent.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(&output_directory, std::fs::Permissions::from_mode(0o700))?;
        let pinned = open_directory_no_follow(&output_directory)?;
        let retained_original = parent.join(format!(
            "{OUTPUT_DIRECTORY_NAME}-original-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&output_directory, &retained_original)?;
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(&output_directory, std::fs::Permissions::from_mode(0o700))?;

        let artifact_path = output_directory.join("descriptor-bound-artifact");
        let mut artifact = create_private_file_new_at(&pinned, &output_directory, &artifact_path)?;
        artifact.write_all(b"pinned-original")?;
        artifact.sync_all()?;
        let identity = FileIdentity::capture(&artifact.metadata()?, Some(15));
        pinned.sync_all()?;
        validate_file_identity_at(&pinned, &output_directory, &artifact_path, identity)?;
        assert_eq!(
            std::fs::read(retained_original.join("descriptor-bound-artifact"))?,
            b"pinned-original"
        );
        assert!(!output_directory.join("descriptor-bound-artifact").exists());
        assert!(
            create_private_file_new_at(
                &pinned,
                &output_directory,
                &parent.join("escaped-artifact"),
            )
            .is_err()
        );

        let retained_replacement = parent.join(format!(
            "{OUTPUT_DIRECTORY_NAME}-replacement-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&output_directory, &retained_replacement)?;
        std::fs::rename(&retained_original, &output_directory)?;

        validate_file_identity_at(&pinned, &output_directory, &artifact_path, identity)?;
        assert!(
            read_directory_names(&pinned)?
                .iter()
                .any(|name| name.as_os_str() == OsStr::new("descriptor-bound-artifact"))
        );
        assert_eq!(std::fs::read(&artifact_path)?, b"pinned-original");
        assert!(
            !retained_replacement
                .join("descriptor-bound-artifact")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn output_key_provisioning_resumes_partial_private_stage_without_partial_final_name()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-key-stage-")?;
        let output_directory = directory.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(&output_directory, std::fs::Permissions::from_mode(0o700))?;
        let stage = output_directory.join(format!("{OUTPUT_KEY_NAME}.provisioning"));
        let readiness = output_directory.join(format!("{OUTPUT_KEY_NAME}.provisioning.ready"));
        let mut stage_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&stage)?;
        stage_file.write_all(b"partial-key-crash-cut")?;
        stage_file.sync_all()?;
        let mut readiness_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&readiness)?;
        readiness_file.write_all(b"partial-ready")?;
        readiness_file.sync_all()?;
        sync_directory(&output_directory)?;

        let token_path = directory.join("guardian.token");
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        let pipeline = GuardianOutputPipeline::open(&token_path, 1, waker)?;
        let key_path = output_directory.join(OUTPUT_KEY_NAME);
        assert_eq!(
            std::fs::metadata(&key_path)?.len(),
            u64::try_from(GuardianOutputCipher::KEY_BYTES)?
        );
        assert!(!stage.exists());
        assert_eq!(std::fs::metadata(&readiness)?.len(), 36);
        let original_key_id = pipeline.cipher.key_id();
        drop(pipeline);

        let reopened_poll = Poll::new()?;
        let reopened_waker = Arc::new(Waker::new(reopened_poll.registry(), Token(1))?);
        let reopened = GuardianOutputPipeline::open(&token_path, 1, reopened_waker)?;
        assert_eq!(reopened.cipher.key_id(), original_key_id);
        Ok(())
    }

    #[test]
    fn partial_final_output_key_fails_closed_without_overwrite()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-key-final-cut-")?;
        let output_directory = directory.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(&output_directory, std::fs::Permissions::from_mode(0o700))?;
        let key_path = output_directory.join(OUTPUT_KEY_NAME);
        let mut key_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)?;
        key_file.write_all(b"partial")?;
        key_file.sync_all()?;
        sync_directory(&output_directory)?;

        let token_path = directory.join("guardian.token");
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        assert!(GuardianOutputPipeline::open(&token_path, 1, waker).is_err());
        assert_eq!(std::fs::read(&key_path)?, b"partial");
        Ok(())
    }

    #[test]
    fn existing_output_key_is_never_loaded_through_a_replaced_directory_name()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-key-parent-swap-")?;
        let output_directory = directory.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(&output_directory, std::fs::Permissions::from_mode(0o700))?;
        let original_key = [0x11_u8; GuardianOutputCipher::KEY_BYTES];
        let mut key = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output_directory.join(OUTPUT_KEY_NAME))?;
        key.write_all(&original_key)?;
        key.sync_all()?;
        drop(key);
        sync_directory(&output_directory)?;
        let pinned = open_directory_no_follow(&output_directory)?;

        let retained = directory.join(format!(
            "{OUTPUT_DIRECTORY_NAME}-retained-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&output_directory, &retained)?;
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(&output_directory, std::fs::Permissions::from_mode(0o700))?;
        let replacement_key = [0x22_u8; GuardianOutputCipher::KEY_BYTES];
        let mut replacement = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output_directory.join(OUTPUT_KEY_NAME))?;
        replacement.write_all(&replacement_key)?;
        replacement.sync_all()?;
        drop(replacement);
        sync_directory(&output_directory)?;

        assert!(load_or_create_output_key(&pinned, &output_directory).is_err());
        assert_eq!(std::fs::read(retained.join(OUTPUT_KEY_NAME))?, original_key);
        assert_eq!(
            std::fs::read(output_directory.join(OUTPUT_KEY_NAME))?,
            replacement_key
        );
        Ok(())
    }

    #[test]
    fn missing_output_key_never_creates_a_split_authority_over_existing_artifacts()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-key-missing-")?;
        let output_directory = directory.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(&output_directory, std::fs::Permissions::from_mode(0o700))?;
        let retained_ciphertext = output_directory.join(
            "00000000-0000-0000-0000-000000000001-00000000-0000-0000-0000-000000000002-segment-00000000-0000-0000-0000-000000000003.ftgout",
        );
        let mut artifact = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&retained_ciphertext)?;
        artifact.write_all(b"retained encrypted crash evidence")?;
        artifact.sync_all()?;
        sync_directory(&output_directory)?;

        let token_path = directory.join("guardian.token");
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        assert!(GuardianOutputPipeline::open(&token_path, 1, waker).is_err());
        assert!(!output_directory.join(OUTPUT_KEY_NAME).exists());
        assert!(
            !output_directory
                .join(format!("{OUTPUT_KEY_NAME}.provisioning"))
                .exists()
        );
        assert_eq!(
            std::fs::read(&retained_ciphertext)?,
            b"retained encrypted crash evidence"
        );
        Ok(())
    }

    #[test]
    fn encrypted_commits_are_ordered_and_recoverable_only_after_sync_receipts()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-order-")?;
        let token_path = directory.join("guardian.token");
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        let pipeline = GuardianOutputPipeline::open(&token_path, 1, waker)?;
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(Uuid::new_v4(), pane_id)?;

        for (expected_sequence, payload) in [(1, b"first".as_slice()), (2, b"second".as_slice())] {
            pipeline
                .try_submit(pane_id, journal.clone(), zeroizing_test_bytes(payload))
                .map_err(|_| "output submission was unexpectedly rejected")?;
            let completion = completion(&pipeline)?;
            let receipt = completion.result.map_err(|_| "durable append failed")?;
            assert_eq!(receipt.sequence(), expected_sequence);
            assert_eq!(usize::try_from(receipt.payload_bytes())?, payload.len());
        }

        let authority = journal
            .authority
            .lock()
            .map_err(|_| "journal authority was poisoned")?;
        let recovered = recover_all_segment_bytes(&authority)?;
        assert_eq!(recovered, b"firstsecond");
        Ok(())
    }

    #[test]
    fn rotation_cold_open_and_replay_preserve_exact_cross_segment_authority()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = tiny_rotation_policy(4);
        let (directory, poll, pipeline) =
            pipeline_with_policy("ft-guardian-output-rotation-", policy)?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        let mut receipts = Vec::new();
        for (sequence, payload) in [
            (1, b"a".as_slice()),
            (2, b"bb".as_slice()),
            (3, b"ccc".as_slice()),
        ] {
            let receipt = durable_commit(&pipeline, pane_id, &journal, payload)?;
            assert_eq!(receipt.sequence(), sequence);
            receipts.push(receipt);
        }

        {
            let authority = journal
                .authority
                .lock()
                .map_err(|_| "journal authority was poisoned")?;
            assert_eq!(authority.segments.len(), 3);
            assert_eq!(authority.manifest.snapshot.revision, 3);
            assert_eq!(authority.manifest_history.len(), 3);
            assert_eq!(authority.physical_segment_files, 3);
            for index in 1..authority.segments.len() {
                assert_eq!(
                    authority.segments[index].segment_identity.predecessor(),
                    Some(receipts[index - 1].into_predecessor())
                );
                assert_eq!(
                    authority.segments[index].segment_identity.first_sequence(),
                    receipts[index - 1].sequence() + 1
                );
            }
            for (index, segment) in authority.segments.iter().enumerate() {
                assert!(authority.segments[..index].iter().all(|prior| {
                    prior.segment_identity.segment_id() != segment.segment_identity.segment_id()
                }));
            }
            assert_eq!(recover_all_segment_bytes(&authority)?, b"abbccc");
        }
        drop(journal);
        drop(pipeline);
        drop(poll);

        let (_reopened_poll, reopened_pipeline) = reopen_pipeline(&directory, policy)?;
        let reopened =
            reopened_pipeline.cold_open_pane_for_validation(guardian_incarnation, pane_id)?;
        let final_receipt = durable_commit(&reopened_pipeline, pane_id, &reopened, b"dddd")?;
        assert_eq!(final_receipt.sequence(), 4);
        assert!(reopened.receipt_is_current(final_receipt));
        let authority = reopened
            .authority
            .lock()
            .map_err(|_| "reopened journal authority was poisoned")?;
        assert_eq!(authority.segments.len(), 4);
        assert_eq!(authority.manifest.snapshot.revision, 4);
        assert_eq!(authority.manifest_history.len(), 4);
        assert_eq!(authority.relevant_files, 12);
        assert_eq!(
            authority.segments[3].segment_identity.predecessor(),
            Some(receipts[2].into_predecessor())
        );
        assert_eq!(recover_all_segment_bytes(&authority)?, b"abbcccdddd");
        Ok(())
    }

    #[test]
    fn log_byte_limit_rolls_over_before_the_frozen_append_seam_rejects_payload()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = OutputSegmentPolicy {
            journal_limits: GuardianOutputJournalLimits {
                max_record_bytes: 64,
                max_log_bytes: 352,
                max_records: 10,
            },
            max_segments: 3,
            max_durable_pane_bytes: 4 * 1024,
        };
        let (_directory, _poll, pipeline) =
            pipeline_with_policy("ft-guardian-output-log-rollover-", policy)?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        let first = durable_commit(&pipeline, pane_id, &journal, &[0x41; 64])?;
        let second = durable_commit(&pipeline, pane_id, &journal, b"b")?;
        assert_eq!(first.sequence(), 1);
        assert_eq!(second.sequence(), 2);

        let authority = journal
            .authority
            .lock()
            .map_err(|_| "journal authority was poisoned")?;
        assert_eq!(authority.segments.len(), 2);
        assert_eq!(
            authority.segments[1].segment_identity.predecessor(),
            Some(first.into_predecessor())
        );
        assert_eq!(authority.manifest.snapshot.revision, 2);
        Ok(())
    }

    #[test]
    fn torn_manifest_rolls_back_to_last_exact_chain_without_reclamation()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) =
            pipeline_with_policy("ft-guardian-output-manifest-cut-", tiny_rotation_policy(4))?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        durable_commit(&pipeline, pane_id, &journal, b"one")?;
        durable_commit(&pipeline, pane_id, &journal, b"two")?;
        let torn = pipeline.publish_torn_manifest_candidate(guardian_incarnation, pane_id, 3)?;
        assert!(torn.exists());
        drop(journal);

        let reopened = pipeline.cold_open_pane_for_validation(guardian_incarnation, pane_id)?;
        {
            let authority = reopened
                .authority
                .lock()
                .map_err(|_| "reopened journal authority was poisoned")?;
            assert_eq!(authority.manifest.snapshot.revision, 2);
            assert_eq!(authority.manifest_history.len(), 2);
            assert_eq!(authority.relevant_files, 7);
            assert_eq!(recover_all_segment_bytes(&authority)?, b"onetwo");
        }
        let third = durable_commit(&pipeline, pane_id, &reopened, b"three")?;
        assert_eq!(third.sequence(), 3);
        assert!(torn.exists());
        drop(reopened);

        let validated = pipeline.cold_open_pane_for_validation(guardian_incarnation, pane_id)?;
        let authority = validated
            .authority
            .lock()
            .map_err(|_| "validated journal authority was poisoned")?;
        assert_eq!(authority.manifest.snapshot.revision, 3);
        assert_eq!(authority.manifest_history.len(), 3);
        assert_eq!(authority.relevant_files, 10);
        assert_eq!(recover_all_segment_bytes(&authority)?, b"onetwothree");
        assert!(
            pipeline
                .relevant_pane_paths(guardian_incarnation, pane_id)?
                .contains(&torn)
        );
        Ok(())
    }

    #[test]
    fn empty_published_spawn_preparation_is_idempotent_but_nonempty_retry_is_blocked()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) =
            pipeline_with_policy("ft-guardian-output-spawn-retry-", tiny_rotation_policy(3))?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let first = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        let (segment_id, manifest_id, manifest_checksum) = {
            let authority = first
                .authority
                .lock()
                .map_err(|_| "initial journal authority was poisoned")?;
            (
                authority.segments[0].segment_identity.segment_id(),
                authority.manifest.snapshot.manifest_id,
                authority.manifest.snapshot.checksum,
            )
        };
        let initial_paths = pipeline.relevant_pane_paths(guardian_incarnation, pane_id)?;
        assert_eq!(initial_paths.len(), 3);
        drop(first);

        // Models PTY spawn or mio registration failure after preparation:
        // the exact empty publication is reopened rather than create_new'd.
        let retry = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        {
            let authority = retry
                .authority
                .lock()
                .map_err(|_| "retry journal authority was poisoned")?;
            assert_eq!(
                authority.segments[0].segment_identity.segment_id(),
                segment_id
            );
            assert_eq!(authority.manifest.snapshot.manifest_id, manifest_id);
            assert_eq!(authority.manifest.snapshot.checksum, manifest_checksum);
            assert_eq!(authority.total_records, 0);
            assert_eq!(authority.relevant_files, 3);
        }
        assert_eq!(
            pipeline.relevant_pane_paths(guardian_incarnation, pane_id)?,
            initial_paths
        );

        durable_commit(&pipeline, pane_id, &retry, b"child-output")?;
        drop(retry);
        assert!(
            pipeline
                .prepare_pane(guardian_incarnation, pane_id)
                .is_err()
        );
        assert_eq!(
            pipeline.relevant_pane_paths(guardian_incarnation, pane_id)?,
            initial_paths
        );
        Ok(())
    }

    #[test]
    fn path_link_change_fails_closed_before_plaintext_is_committed()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-link-")?;
        let token_path = directory.join("guardian.token");
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        let pipeline = GuardianOutputPipeline::open(&token_path, 1, waker)?;
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(Uuid::new_v4(), pane_id)?;
        let journal_path = journal
            .authority
            .lock()
            .map_err(|_| "journal authority was poisoned")?
            .segments
            .last()
            .ok_or("journal segment path disappeared")?
            .path
            .clone();
        let extra_link = directory.join("guardian-output-hardlink-evidence");
        hard_link(&journal_path, &extra_link)?;

        pipeline
            .try_submit(
                pane_id,
                journal.clone(),
                zeroizing_test_bytes(b"must-not-commit"),
            )
            .map_err(|_| "output submission was unexpectedly rejected")?;
        assert!(completion(&pipeline)?.result.is_err());
        let authority = journal
            .authority
            .lock()
            .map_err(|_| "journal authority was poisoned")?;
        assert!(authority.failed);
        assert_eq!(authority.current_journal.record_count(), 0);
        Ok(())
    }

    #[test]
    fn manifest_hardlink_is_rejected_on_cold_open_and_retained_as_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, _poll, pipeline) =
            pipeline_with_policy("ft-guardian-output-manifest-link-", tiny_rotation_policy(3))?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        let manifest_path = journal
            .authority
            .lock()
            .map_err(|_| "journal authority was poisoned")?
            .manifest
            .path
            .clone();
        let evidence_link = directory.join("guardian-output-manifest-hardlink-evidence");
        hard_link(&manifest_path, &evidence_link)?;
        drop(journal);

        assert!(
            pipeline
                .cold_open_pane_for_validation(guardian_incarnation, pane_id)
                .is_err()
        );
        assert_eq!(std::fs::metadata(&manifest_path)?.nlink(), 2);
        assert!(manifest_path.exists());
        assert!(evidence_link.exists());
        Ok(())
    }

    #[test]
    fn marked_manifest_checksum_corruption_fails_closed_instead_of_rolling_back()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-manifest-corrupt-",
            tiny_rotation_policy(3),
        )?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        let (manifest_path, publication_path, manifest_bytes) = {
            let authority = journal
                .authority
                .lock()
                .map_err(|_| "journal authority was poisoned")?;
            (
                authority.manifest.path.clone(),
                authority.manifest.publication_path.clone(),
                authority
                    .manifest
                    .file_identity
                    .expected_len
                    .ok_or("published manifest lost its exact length authority")?,
            )
        };
        let mut manifest = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&manifest_path)?;
        manifest.seek(SeekFrom::Start(0))?;
        manifest.write_all(b"X")?;
        manifest.sync_all()?;
        drop(manifest);
        drop(journal);

        assert!(
            pipeline
                .cold_open_pane_for_validation(guardian_incarnation, pane_id)
                .is_err()
        );
        assert_eq!(std::fs::metadata(&manifest_path)?.len(), manifest_bytes);
        assert!(manifest_path.exists());
        assert!(publication_path.exists());
        Ok(())
    }

    #[test]
    fn orphan_publication_marker_fails_closed_and_is_never_reclaimed()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-orphan-publication-",
            tiny_rotation_policy(3),
        )?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        let orphan = manifest_publication_path(
            &pipeline.directory_path,
            guardian_incarnation,
            pane_id,
            2,
            Uuid::new_v4(),
            [0; OUTPUT_MANIFEST_CHECKSUM_BYTES],
        );
        let marker =
            create_private_file_new_at(&pipeline.directory, &pipeline.directory_path, &orphan)?;
        marker.sync_all()?;
        pipeline.directory.sync_all()?;
        drop(marker);
        drop(journal);

        assert!(
            pipeline
                .cold_open_pane_for_validation(guardian_incarnation, pane_id)
                .is_err()
        );
        assert!(orphan.exists());
        Ok(())
    }

    #[test]
    fn preexisting_symlink_cannot_capture_a_collision_resistant_segment_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, _poll, pipeline) =
            pipeline_with_policy("ft-guardian-output-symlink-", tiny_rotation_policy(3))?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let segment_id = Uuid::new_v4();
        let identity = GuardianOutputSegmentIdentity::new(pane_id, segment_id, 1, None)?;
        let target = directory.join("attacker-target");
        let mut target_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        target_file.write_all(b"unchanged")?;
        target_file.sync_all()?;
        let collision_path = segment_path(
            &pipeline.directory_path,
            guardian_incarnation,
            pane_id,
            segment_id,
        );
        symlink(&target, &collision_path)?;

        assert!(
            create_segment_at_identity(
                &pipeline.directory,
                &pipeline.directory_path,
                guardian_incarnation,
                identity,
                pipeline.cipher.clone(),
                pipeline.policy.journal_limits,
            )
            .is_err()
        );
        assert!(
            std::fs::symlink_metadata(&collision_path)?
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target)?, b"unchanged");
        Ok(())
    }

    #[test]
    fn segment_count_exhaustion_fails_closed_without_creating_or_reclaiming_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) =
            pipeline_with_policy("ft-guardian-output-capacity-", tiny_rotation_policy(2))?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        assert_eq!(
            durable_commit(&pipeline, pane_id, &journal, b"one")?.sequence(),
            1
        );
        assert_eq!(
            durable_commit(&pipeline, pane_id, &journal, b"two")?.sequence(),
            2
        );
        assert!(!journal.can_accept_min_record());
        let paths_at_capacity = pipeline.relevant_pane_paths(guardian_incarnation, pane_id)?;
        assert_eq!(paths_at_capacity.len(), 6);

        pipeline
            .try_submit(
                pane_id,
                journal.clone(),
                zeroizing_test_bytes(b"must-not-commit"),
            )
            .map_err(|_| "capacity probe submission was unexpectedly rejected")?;
        assert!(completion(&pipeline)?.result.is_err());
        assert_eq!(
            pipeline.relevant_pane_paths(guardian_incarnation, pane_id)?,
            paths_at_capacity
        );
        let authority = journal
            .authority
            .lock()
            .map_err(|_| "journal authority was poisoned")?;
        assert_eq!(authority.total_records, 2);
        assert_eq!(authority.segments.len(), 2);
        assert_eq!(authority.physical_segment_files, 2);
        assert_eq!(recover_all_segment_bytes(&authority)?, b"onetwo");
        Ok(())
    }

    #[test]
    fn total_disk_byte_bound_stops_admission_before_an_extra_record_or_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = OutputSegmentPolicy {
            journal_limits: GuardianOutputJournalLimits {
                max_record_bytes: 64,
                max_log_bytes: 512,
                max_records: 10,
            },
            max_segments: 4,
            max_durable_pane_bytes: 620,
        };
        let (_directory, _poll, pipeline) =
            pipeline_with_policy("ft-guardian-output-disk-cap-", policy)?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        let payload = [0x5a; 64];
        assert_eq!(
            durable_commit(&pipeline, pane_id, &journal, &payload)?.sequence(),
            1
        );
        assert!(!journal.can_accept_min_record());
        let paths_at_capacity = pipeline.relevant_pane_paths(guardian_incarnation, pane_id)?;
        assert_eq!(paths_at_capacity.len(), 3);

        pipeline
            .try_submit(pane_id, journal.clone(), zeroizing_test_bytes(b"x"))
            .map_err(|_| "disk capacity probe submission was unexpectedly rejected")?;
        assert!(completion(&pipeline)?.result.is_err());
        assert_eq!(
            pipeline.relevant_pane_paths(guardian_incarnation, pane_id)?,
            paths_at_capacity
        );
        let authority = journal
            .authority
            .lock()
            .map_err(|_| "journal authority was poisoned")?;
        assert_eq!(authority.total_relevant_file_bytes, 620);
        assert_eq!(authority.total_records, 1);
        Ok(())
    }

    #[test]
    fn incomplete_segment_tail_is_rejected_without_truncation_or_reclamation()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) =
            pipeline_with_policy("ft-guardian-output-segment-cut-", tiny_rotation_policy(3))?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        let receipt = durable_commit(&pipeline, pane_id, &journal, b"committed")?;
        let segment_path = journal
            .authority
            .lock()
            .map_err(|_| "journal authority was poisoned")?
            .segments[0]
            .path
            .clone();
        let mut segment = OpenOptions::new().append(true).open(&segment_path)?;
        segment.write_all(b"torn")?;
        segment.sync_all()?;
        let torn_bytes = receipt.committed_log_bytes() + 4;
        assert_eq!(segment.metadata()?.len(), torn_bytes);
        drop(segment);
        drop(journal);

        assert!(
            pipeline
                .cold_open_pane_for_validation(guardian_incarnation, pane_id)
                .is_err()
        );
        assert_eq!(std::fs::metadata(&segment_path)?.len(), torn_bytes);
        assert!(segment_path.exists());
        Ok(())
    }

    #[test]
    fn queue_admission_is_bounded_even_without_a_draining_worker()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-bound-")?;
        let token_path = directory.join("guardian.token");
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        let pipeline = GuardianOutputPipeline::open(&token_path, 1, waker)?;
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(Uuid::new_v4(), pane_id)?;
        let queue = OutputQueue::new(1)?;
        let first = OutputJob {
            pane_id,
            journal: journal.clone(),
            payload: zeroizing_test_bytes(b"reserved"),
        };
        assert!(queue.try_push(first).is_ok());
        let second = OutputJob {
            pane_id,
            journal,
            payload: zeroizing_test_bytes(b"backpressured"),
        };
        let OutputQueuePushError::Saturated(mut second) = queue
            .try_push(second)
            .expect_err("full queue must reject atomically as saturated")
        else {
            return Err("full live queue was misclassified as shut down".into());
        };
        assert_eq!(second.payload.as_slice(), b"backpressured");
        second.payload.zeroize();
        assert_eq!(queue.available_slots(), 0);
        let mut retained = queue.pop().ok_or("reserved job disappeared")?;
        retained.payload.zeroize();
        assert!(queue.complete_one());
        assert_eq!(queue.available_slots(), 1);
        queue.shutdown();
        let after_shutdown = OutputJob {
            pane_id,
            journal: retained.journal,
            payload: zeroizing_test_bytes(b"unavailable"),
        };
        let OutputQueuePushError::Shutdown(mut after_shutdown) = queue
            .try_push(after_shutdown)
            .expect_err("shut-down queue must report permanent unavailability")
        else {
            return Err("shut-down queue was misclassified as transient saturation".into());
        };
        assert_eq!(after_shutdown.payload.as_slice(), b"unavailable");
        after_shutdown.payload.zeroize();
        Ok(())
    }
}
