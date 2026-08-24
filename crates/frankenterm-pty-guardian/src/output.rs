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
    GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES, GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES,
    GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
    GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES, GuardianCheckpointArtifactDescriptorV1,
    GuardianCheckpointBoundaryError, GuardianCheckpointCandidateIdentityV1,
    GuardianCheckpointCipher, GuardianCheckpointCipherError,
    GuardianCheckpointOrderedChunkSetBuilderV1, GuardianCheckpointOrderedChunkSetIdentityV1,
    GuardianCheckpointStageBindingV1, GuardianCheckpointStageRecordContextV1,
    GuardianCheckpointStageRecordKindV1, GuardianCheckpointStageScopeV1,
    GuardianCheckpointStageSealIntentV1, GuardianCheckpointValidatedManifestAuthorityV1,
    GuardianEncryptedCheckpointStageRecordV1,
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
    AuthenticatedGuardianRequest, GUARDIAN_MAX_CHECKPOINT_BYTES,
    GUARDIAN_MAX_CHECKPOINT_CHUNKS, GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES,
    GuardianCheckpointDescriptorV1, GuardianCheckpointScopeV1, GuardianCheckpointStageKindV1,
    GuardianCheckpointStageReplyV1, GuardianCheckpointStageRequestV1,
    GuardianEffectTransactionError, GuardianProtocolError, GuardianProtocolState, GuardianReply,
};
use nix::unistd::{PathconfVar, fpathconf, geteuid};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use thiserror::Error;
use uuid::Uuid;
use zeroize::{Zeroize as _, Zeroizing};

pub(crate) const OUTPUT_RECORD_BYTES: usize = 8 * 1024;
const OUTPUT_DIRECTORY_NAME: &str = "guardian-output-v3";
const OUTPUT_KEY_NAME: &str = "journal.key";
const INPUT_JOURNAL_SUFFIX: &str = "ftgin";
const OUTPUT_WORKER_THREADS: usize = 2;
const OUTPUT_MAX_IN_FLIGHT: usize = 64;
const OUTPUT_MANIFEST_CHECKSUM_DOMAIN: &[u8] =
    b"frankenterm.guardian-output-manifest.v1\0";
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
const CHECKPOINT_STAGE_RECORD_OVERHEAD_BYTES: u64 = 296;
const CHECKPOINT_STAGE_MAX_FILES_PER_UPLOAD: usize = 1_026;
const CHECKPOINT_STAGE_MAX_BYTES_PER_UPLOAD: u64 = 268_739_888;
// Phase A has no deletion or ACK-backed reclamation. Keep this deliberately
// finite; whole-fleet activation remains disabled until the catalog layer can
// retain one proven generation per pane without exhausting this quarantine.
const CHECKPOINT_STAGE_MAX_RETAINED_UPLOADS: usize = 8;
const CHECKPOINT_STAGE_MAX_FILES: usize = 8_208;
const CHECKPOINT_STAGE_MAX_BYTES: u64 = 2_149_919_104;

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
            .and_then(|chunks| chunks.checked_add(2))
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
pub(crate) enum GuardianOutputError {
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
pub(crate) enum GuardianCheckpointStageStoreError {
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
    #[error("guardian checkpoint staging publication authority is unavailable")]
    PublicationAuthorityUnavailable,
}

impl GuardianCheckpointStageStoreError {
    fn io(site: &'static str, source: std::io::Error) -> Self {
        Self::Io { site, source }
    }
}

// Phase A constructs the durable store but deliberately does not route wire
// requests into it until the runtime supplies the non-forgeable authority
// boundary required by final publication.
#[allow(dead_code)]
pub(crate) enum GuardianCheckpointOriginAuthority<'a> {
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
            } => format!(
                "checkpoint-pane-{pane_id}.generation-{generation:020}.upload-{upload_id}"
            ),
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

/// Descriptor-relative, synchronously durable Phase-A checkpoint staging.
///
/// These methods perform bounded filesystem and AEAD work and therefore must
/// run only on the dedicated checkpoint worker, never on the Mio readiness
/// loop. Clones share one process-local gate; a directory `flock` coordinates
/// cooperating guardian processes before deterministic `O_EXCL` publication.
#[derive(Clone)]
pub(crate) struct GuardianCheckpointStageStore {
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

#[derive(Debug)]
struct PersistentOutputAuthority {
    parent_path: PathBuf,
    parent_identity: FileIdentity,
    directory_path: PathBuf,
    directory_identity: FileIdentity,
    key_path: PathBuf,
    key_identity: FileIdentity,
}

impl PersistentOutputAuthority {
    fn validate(&self, directory: &File) -> Result<(), OutputCommitError> {
        validate_path_identity(&self.parent_path, self.parent_identity)
            .map_err(|_| OutputCommitError::PersistenceAuthority)?;
        validate_path_identity(&self.directory_path, self.directory_identity)
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
            || manifest_tail.publication_file_identity
                != self.manifest.publication_file_identity
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
        let next_manifest_bytes = manifest_encoded_bytes(next_segment_count)
            .map_err(|_| OutputCommitError::Capacity)?;
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
            && self
                .segments
                .last()
                .is_some_and(|segment| segment.segment_identity.segment_id() == receipt.segment_id())
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
        if journal_can_append(
            &self.current_journal,
            self.policy.journal_limits,
            1,
        )
        .unwrap_or(false)
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
pub(crate) struct GuardianPaneOutputJournal {
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
        persistence
            .validate(directory)
            .map_err(|_| GuardianOutputError::FilesystemAuthority(
                "guardian checkpoint persistence authority changed during initialization",
            ))?;
        let name_max = checkpoint_stage_name_max(directory)?;
        if name_max < checkpoint_stage_longest_name_bytes() {
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
                entry.key == shape.key()
                    && entry.role == CheckpointStageFileRole::Candidate
            });
            let has_any = census
                .entries
                .iter()
                .any(|entry| entry.key == shape.key());
            if !has_candidate {
                if has_any {
                    return Err(GuardianCheckpointStageStoreError::Poisoned);
                }
                let publication_id = Uuid::new_v4();
                let record_bytes = checkpoint_record_bytes_for_plaintext(begin_payload.len())?;
                checkpoint_stage_require_capacity(
                    inner,
                    &census,
                    shape.key(),
                    1,
                    record_bytes,
                )?;
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
        let (index, offset) = chunk.position();
        let observed_digest = checkpoint_zeroizing_sha256_digest(chunk.bytes());
        if !checkpoint_bytes_match(observed_digest.as_slice(), chunk.chunk_digest()) {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        let bytes = chunk.into_bytes();
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
            checkpoint_stage_require_capacity(
                inner,
                &census,
                shape.key(),
                1,
                record_bytes,
            )?;
            let path = checkpoint_chunk_path(
                inner,
                shape.key(),
                inspection.publication_id,
                index,
            )?;
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

    pub(crate) fn apply_seal(
        &self,
        request: &GuardianCheckpointStageRequestV1,
        origin_authority: GuardianCheckpointOriginAuthority<'_>,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianCheckpointStageStoreError> {
        if request.kind() != GuardianCheckpointStageKindV1::Seal {
            return Err(GuardianCheckpointStageStoreError::Conflict);
        }
        let shape = CheckpointStageRequestShape::from_request(request)?;
        self.with_exclusive_directory(|inner| {
            let census = checkpoint_stage_census(inner)?;
            let inspection = checkpoint_inspect_upload(
                inner,
                &census,
                &shape,
                CheckpointStageSealInspection::Reject,
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
            let payload = checkpoint_assemble_payload(
                inner,
                &census,
                &shape,
                inspection.publication_id,
            )?;
            request.validate_staged_plaintext(payload.as_slice())?;
            let _manifest_authority = match origin_authority {
                GuardianCheckpointOriginAuthority::Record {
                    journal,
                    manifest_authority,
                }
                    if matches!(shape.path_scope, CheckpointStagePathScope::Pane { .. }) =>
                {
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
            let _validated_logical_components = (
                inspection.candidate_identity,
                ordered_chunk_set_identity,
            );
            // The filesystem inspection above derives the cipher-owned opaque
            // logical identities for the exact assembly. Those identities and
            // raw request fields still cannot mint the independent,
            // nonconstructible assembly witness. Until the authenticated
            // runtime/journal boundary issues that witness, canonical 400-byte
            // manifest construction, final publication, and adoption remain
            // deliberately unavailable.
            Err(GuardianCheckpointStageStoreError::PublicationAuthorityUnavailable)
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
        let mut directory_lock =
            CheckpointStageDirectoryLock::exclusive(&self.inner.directory)?;
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
    rustix::fs::flock(directory, rustix::fs::FlockOperation::Unlock)
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
    usize::try_from(observed).map_err(|_| GuardianOutputError::FilesystemAuthority(
        "guardian checkpoint directory name bound is invalid",
    ))
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
) -> Result<
    (CheckpointStageUploadKey, CheckpointStageFileRole),
    GuardianCheckpointStageStoreError,
> {
    if !raw.is_ascii() {
        return Err(GuardianCheckpointStageStoreError::Poisoned);
    }
    let name = std::str::from_utf8(raw)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
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
        (
            CheckpointStagePathScope::Genesis { spawn_effect_id },
            rest,
        )
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
            if index >= GUARDIAN_MAX_CHECKPOINT_CHUNKS
                || format!("{index:010}") != index_text
            {
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
    Ok((
        CheckpointStageUploadKey {
            scope,
            upload_id,
        },
        role,
    ))
}

fn checkpoint_take_uuid(
    value: &str,
) -> Result<(Uuid, &str), GuardianCheckpointStageStoreError> {
    let encoded = value
        .get(..36)
        .ok_or(GuardianCheckpointStageStoreError::Poisoned)?;
    let identity = Uuid::parse_str(encoded)
        .map_err(|_| GuardianCheckpointStageStoreError::Poisoned)?;
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
    file.sync_all().map_err(|error| {
        GuardianCheckpointStageStoreError::io("checkpoint-record-sync", error)
    })?;
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
    validate_file_identity_at(
        &inner.directory,
        &inner.directory_path,
        path,
        identity,
    )?;
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

fn checkpoint_open_record(
    inner: &GuardianCheckpointStageStoreInner,
    entry: &CheckpointStageCensusEntry,
    max_plaintext_bytes: u32,
) -> Result<
    (
        GuardianCheckpointStageRecordContextV1,
        Zeroizing<Vec<u8>>,
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
    let mut file = open_private_file_at(
        &inner.directory,
        &inner.directory_path,
        &entry.path,
        false,
    )?;
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
            GuardianCheckpointStageStoreError::io(
                "checkpoint-record-retry-directory-sync",
                error,
            )
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
        }
    }
    let Some(candidate) = candidate else {
        return if chunks.is_empty() && seal.is_none() {
            Ok(None)
        } else {
            Err(GuardianCheckpointStageStoreError::Poisoned)
        };
    };
    let seal_present = seal.is_some();
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
            CheckpointStageFileRole::Candidate | CheckpointStageFileRole::Seal { .. } => {
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
    let next_index = u32::try_from(chunks.len())
        .map_err(|_| GuardianCheckpointStageStoreError::Capacity)?;
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
    let (context, observed_plaintext) =
        checkpoint_open_record(inner, entry, max_plaintext_bytes)?;
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
        .fold(0_u8, |difference, (left, right)| difference | (*left ^ *right))
        == 0
}

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
pub(crate) struct GuardianPaneInputJournal {
    journal: GuardianInputJournal,
    directory: File,
    directory_path: PathBuf,
    path: PathBuf,
    file_identity: FileIdentity,
    persistence: Arc<PersistentOutputAuthority>,
}

pub(crate) type GuardianPaneInputTransaction = GuardianInputTransaction;

pub(crate) enum GuardianPaneInputTransactionError {
    Protocol(GuardianProtocolError),
    JournalBeforeWrite,
    AuthorityBeforeWrite,
    OutcomeIndeterminate,
    AcceptedJournalUnavailable(GuardianReply),
    AcceptedAuthorityUnavailable,
    AcceptedProtocolUnavailable(GuardianReply),
}

pub(crate) enum GuardianPaneInputCompletionError {
    DispositionIndeterminate,
    Journal,
    Authority,
    Protocol,
}

impl GuardianPaneInputJournal {
    fn validate_path_authority(&self) -> Result<(), GuardianOutputError> {
        self.persistence
            .validate(&self.directory)
            .map_err(|_| {
                GuardianOutputError::FilesystemAuthority(
                    "guardian input persistence authority changed",
                )
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
                self.validate_path_authority().map_err(|_| {
                    GuardianPaneInputTransactionError::AcceptedAuthorityUnavailable
                })?;
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
                accepted_reply,
                error: _,
            }) => {
                self.validate_path_authority().map_err(|_| {
                    GuardianPaneInputTransactionError::AcceptedAuthorityUnavailable
                })?;
                Err(GuardianPaneInputTransactionError::AcceptedJournalUnavailable(
                    accepted_reply,
                ))
            }
            Err(GuardianInputTransactionError::AcceptedProtocolUnavailable {
                accepted_reply,
                error: _,
            }) => {
                self.validate_path_authority().map_err(|_| {
                    GuardianPaneInputTransactionError::AcceptedAuthorityUnavailable
                })?;
                Err(GuardianPaneInputTransactionError::AcceptedProtocolUnavailable(
                    accepted_reply,
                ))
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

pub(crate) struct GuardianOutputCompletion {
    pub(crate) pane_id: Uuid,
    pub(crate) payload_bytes: usize,
    pub(crate) result: Result<GuardianOutputAppendReceipt, GuardianOutputCommitFailure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GuardianOutputCommitFailure;

pub(crate) enum GuardianOutputCompletionState {
    Ready(GuardianOutputCompletion),
    Empty,
    Disconnected,
}

pub(crate) enum GuardianOutputSubmitError {
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
        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
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
        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
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
        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.outstanding == 0 {
            state.shutdown = true;
            self.ready.notify_all();
            return false;
        }
        state.outstanding -= 1;
        true
    }

    fn available_slots(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.shutdown {
            0
        } else {
            state.max_outstanding.saturating_sub(state.outstanding)
        }
    }

    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
        state.shutdown = true;
        while let Some(mut job) = state.jobs.pop_front() {
            job.payload.zeroize();
            state.outstanding = state.outstanding.saturating_sub(1);
        }
        self.ready.notify_all();
    }
}

/// Fixed-size worker pool plus secure per-pane journal factory.
pub(crate) struct GuardianOutputPipeline {
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
        let (cipher, key_path, key_identity) = load_or_create_output_key(
            &directory,
            &directory_path,
        )?;
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
        let max_outstanding = max_panes.min(OUTPUT_MAX_IN_FLIGHT).max(1);
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
                    output_worker(
                        &worker_queue,
                        &worker_completions,
                        &worker_waker,
                    );
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
        self.persistence
            .validate(&self.directory)
            .map_err(|_| GuardianOutputError::FilesystemAuthority(
                "guardian output persistence authority changed",
            ))?;
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
        self.persistence
            .validate(&self.directory)
            .map_err(|_| {
                GuardianOutputError::FilesystemAuthority(
                    "guardian input persistence authority changed",
                )
            })?;
        let directory = self
            .directory
            .try_clone()
            .map_err(|error| GuardianOutputError::io("input-directory-clone", error))?;
        let path = input_journal_path(&self.directory_path, guardian_incarnation, pane_id);
        let (file, created) = match open_private_file_at(
            &directory,
            &self.directory_path,
            &path,
            false,
        ) {
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
        validate_file_identity_at(
            &directory,
            &self.directory_path,
            &path,
            file_identity,
        )?;
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
        list_relevant_pane_paths(
            &self.directory_path,
            guardian_incarnation,
            pane_id,
        )
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
        let mut file = create_private_file_new_at(
            &self.directory,
            &self.directory_path,
            &path,
        )?;
        file.write_all(b"torn-manifest-crash-cut")
            .map_err(|error| GuardianOutputError::io("output-manifest-crash-cut-write", error))?;
        file.sync_all()
            .map_err(|error| GuardianOutputError::io("output-manifest-crash-cut-sync", error))?;
        self.directory
            .sync_all()
            .map_err(|error| GuardianOutputError::io("output-manifest-crash-cut-dir-sync", error))?;
        Ok(path)
    }
}

fn pane_journal_handle(
    authority: PaneJournalAuthority,
) -> Result<GuardianPaneOutputJournal, GuardianOutputError> {
    let initial_next_sequence = authority.current_journal.next_sequence();
    let initial_cumulative_plaintext_bytes =
        authority.current_journal.cumulative_plaintext_bytes();
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

fn input_journal_path(
    directory_path: &Path,
    guardian_incarnation: Uuid,
    pane_id: Uuid,
) -> PathBuf {
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
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
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
        let identity = GuardianOutputSegmentIdentity::new(
            pane_id,
            segment_id,
            first_sequence,
            predecessor,
        )?;
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
        let publication =
            create_private_file_new_at(directory, directory_path, &publication_path)?;
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

fn encode_manifest(
    snapshot: &mut OutputManifestSnapshot,
) -> Result<Vec<u8>, GuardianOutputError> {
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
        let source = self
            .bytes
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
        .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
        != 0
    {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output manifest checksum mismatch",
        ));
    }
    let mut decoder = ManifestDecoder::new(&bytes[..checksum_offset]);
    if decoder.take::<8>()? != OUTPUT_MANIFEST_MAGIC
        || decoder.u32()? != OUTPUT_MANIFEST_VERSION
    {
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
            && predecessor_checksum == [0; OUTPUT_MANIFEST_CHECKSUM_BYTES] => None,
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
    let segment_count = usize::try_from(decoder.u32()?)
        .map_err(|_| GuardianOutputError::Allocation)?;
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
                && previous_committed == 0 => None,
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
        let opened_metadata = opened
            .metadata()
            .map_err(|error| GuardianOutputError::io("output-publication-opened-metadata", error))?;
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
            validate_file_identity_at(
                directory,
                directory_path,
                &path,
                manifest_identity,
            )?;
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
    let manifests = pair_manifest_publications(
        &mut manifest_candidates,
        manifest_publications,
    )?;
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
        let snapshot = candidate
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
) -> Result<
    (Option<DiscoveredManifest>, Vec<ManifestPathAuthority>),
    GuardianOutputError,
> {
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
            || candidate.snapshot.segments.len()
                != head.snapshot.segments.len().saturating_add(1)
            || candidate.snapshot.segments[..head.snapshot.segments.len()]
                != head.snapshot.segments
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
    (remainder
        == format!(
            "manifest-{revision:020}-{manifest_id}.ftgmanifest"
        ))
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
    persistence
        .validate(directory)
        .map_err(|_| GuardianOutputError::FilesystemAuthority(
            "guardian output persistence authority changed before pane open",
        ))?;
    let scan = scan_pane_publications(
        directory,
        directory_path,
        guardian_incarnation,
        pane_id,
    )?;
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
    authority
        .validate_path_authority()
        .map_err(|_| GuardianOutputError::FilesystemAuthority(
            "guardian output initial publication authority changed",
        ))?;
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
    persistence
        .validate(directory)
        .map_err(|_| GuardianOutputError::FilesystemAuthority(
            "guardian output persistence authority changed before cold open",
        ))?;
    let scan = scan_pane_publications(
        directory,
        directory_path,
        guardian_incarnation,
        pane_id,
    )?;
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
    authority
        .validate_path_authority()
        .map_err(|_| GuardianOutputError::FilesystemAuthority(
            "guardian output cold-open publication authority changed",
        ))?;
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
    let _ = open_and_validate_segment_chain(
        directory,
        directory_path,
        segments,
        cipher,
        policy,
    )?;
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
            record
                .into_authenticated_delivery()?
                .write_all_bounded(
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
        let entry = entry
            .map_err(|error| GuardianOutputError::io("output-test-directory-entry", error))?;
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
) -> Result<(File, PathBuf, FileIdentity, FileIdentity), GuardianOutputError> {
    validate_normalized_absolute_file_path(token_path)?;
    let parent = token_path.parent().ok_or(GuardianOutputError::InvalidPath)?;
    validate_private_directory(parent)?;
    let parent_before = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianOutputError::io("output-parent-metadata-before", error))?;
    validate_private_directory_metadata(&parent_before)?;
    let parent_directory = open_directory_no_follow(parent)?;
    let parent_opened = parent_directory
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-parent-opened-metadata", error))?;
    validate_private_directory_metadata(&parent_opened)?;
    require_same_identity(&parent_before, &parent_opened, None)?;
    let parent_after_open = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianOutputError::io("output-parent-metadata-after-open", error))?;
    require_same_identity(&parent_opened, &parent_after_open, None)?;

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
    let rebound_directory = open_private_directory_at(
        &parent_directory,
        OsStr::new(OUTPUT_DIRECTORY_NAME),
    )
    .map_err(|error| GuardianOutputError::io("output-directory-reopen-at", error))?;
    let rebound_metadata = rebound_directory
        .metadata()
        .map_err(|error| GuardianOutputError::io("output-directory-reopened-metadata", error))?;
    validate_private_directory_metadata(&rebound_metadata)?;
    require_same_identity(&opened_directory, &rebound_metadata, None)?;
    let parent_after = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianOutputError::io("output-parent-metadata-after", error))?;
    require_same_identity(&parent_opened, &parent_after, None)?;
    let path_directory = std::fs::symlink_metadata(&directory_path)
        .map_err(|error| GuardianOutputError::io("output-directory-path-metadata", error))?;
    require_same_identity(&opened_directory, &path_directory, None)?;
    Ok((
        directory,
        directory_path,
        FileIdentity::capture(&parent_opened, None),
        FileIdentity::capture(&opened_directory, None),
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
        Err(GuardianOutputError::Io { source, .. })
            if source.kind() == ErrorKind::NotFound =>
        {
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
    require_same_identity(&opened, &opened_after, Some(expected_len))?;
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
    require_same_identity(&opened, &named, None)
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
    require_same_identity(&opened_before, &named_before, None)?;

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
    require_same_identity(&opened_before, &opened_after, None)?;
    require_same_identity(&opened_after, &named_after, None)?;
    if !found_non_provisioning_entry {
        return Ok(());
    }

    // A concurrent provisioner may have installed the exact private key before
    // publishing its first pane artifact. Accept only that completed authority;
    // otherwise ciphertext/manifest evidence without its key must block creation
    // of a split replacement authority.
    match open_private_file_at(directory, directory_path, key_path, false) {
        Ok(file) => validate_private_file_metadata(
            &file
                .metadata()
                .map_err(|error| GuardianOutputError::io("output-key-census-final-metadata", error))?,
            Some(expected_key_bytes),
        ),
        Err(GuardianOutputError::Io { source, .. })
            if source.kind() == ErrorKind::NotFound =>
        {
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
    let mut flags = rustix::fs::OFlags::RDWR
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW;
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
) -> Result<(), GuardianOutputError> {
    if !FileIdentity::capture(left, expected_len).matches(right) {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output filesystem identity changed during open",
        ));
    }
    Ok(())
}

fn validate_path_identity(
    path: &Path,
    identity: FileIdentity,
) -> Result<(), GuardianOutputError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| GuardianOutputError::io("output-authority-revalidation", error))?;
    if !identity.matches(&metadata) {
        return Err(GuardianOutputError::FilesystemAuthority(
            "guardian output filesystem identity changed",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio::{Poll, Token};
    use std::fs::hard_link;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt, symlink};
    use std::time::{Duration, Instant};

    fn zeroizing_test_bytes(bytes: &[u8]) -> Zeroizing<Vec<u8>> {
        let mut owned = Zeroizing::new(Vec::with_capacity(bytes.len()));
        owned.extend_from_slice(bytes);
        owned
    }

    fn kept_private_directory(prefix: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let canonical_temp = std::fs::canonicalize(std::env::temp_dir())?;
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
        let pipeline = GuardianOutputPipeline::open_with_policy(
            &token_path,
            1,
            waker,
            policy,
        )?;
        Ok((directory, poll, pipeline))
    }

    fn reopen_pipeline(
        directory: &Path,
        policy: OutputSegmentPolicy,
    ) -> Result<(Poll, GuardianOutputPipeline), Box<dyn std::error::Error>> {
        let token_path = directory.join("guardian.token");
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        let pipeline = GuardianOutputPipeline::open_with_policy(
            &token_path,
            1,
            waker,
            policy,
        )?;
        Ok((poll, pipeline))
    }

    fn durable_commit(
        pipeline: &GuardianOutputPipeline,
        pane_id: Uuid,
        journal: &GuardianPaneOutputJournal,
        payload: &[u8],
    ) -> Result<GuardianOutputAppendReceipt, Box<dyn std::error::Error>> {
        pipeline
            .try_submit(
                pane_id,
                journal.clone(),
                zeroizing_test_bytes(payload),
            )
            .map_err(|_| "output submission was unexpectedly rejected")?;
        completion(pipeline)?
            .result
            .map_err(|_| "durable append failed".into())
    }

    fn checkpoint_test_terminal_digest(payload: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"frankenterm.guardian-checkpoint-terminal-payload.v1\0");
        hasher.update(u64::try_from(payload.len()).expect("fixture length fits u64").to_le_bytes());
        hasher.update(payload);
        hasher.finalize().into()
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
        boundary_hasher.update(
            b"frankenterm.guardian-checkpoint-genesis-boundary-identity.v1\0",
        );
        boundary_hasher.update(spawn_effect_id.as_bytes());
        let boundary_digest: [u8; 32] = boundary_hasher.finalize().into();
        let mut checkpoint_hasher = Sha256::new();
        checkpoint_hasher.update(
            b"frankenterm.guardian-checkpoint-artifact-identity.v1\0",
        );
        checkpoint_hasher.update(boundary_digest);
        checkpoint_hasher.update(0_u64.to_le_bytes());
        checkpoint_hasher.update(replay_identity);
        checkpoint_hasher.update(24_u32.to_le_bytes());
        checkpoint_hasher.update(80_u32.to_le_bytes());
        checkpoint_hasher.update(total_bytes.to_le_bytes());
        checkpoint_hasher.update(terminal_digest);
        let checkpoint_digest: [u8; 32] = checkpoint_hasher.finalize().into();

        let mut wire: Zeroizing<Vec<u8>> = Zeroizing::new(vec![
            0_u8;
            CHECKPOINT_STAGE_CANDIDATE_PLAINTEXT_BYTES
        ]);
        wire[..4].copy_from_slice(b"GCS1");
        wire[4..6].copy_from_slice(&1_u16.to_be_bytes());
        wire[6] = match kind {
            GuardianCheckpointStageKindV1::Begin => 1,
            GuardianCheckpointStageKindV1::Chunk => 2,
            GuardianCheckpointStageKindV1::Seal => 3,
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
            (GuardianCheckpointStageKindV1::Begin | GuardianCheckpointStageKindV1::Seal, None) => {}
            _ => return Err(GuardianProtocolError::InvalidOperationPayload),
        }
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

        let uppercase = chunk_name.replace(
            &pane_id.to_string(),
            &pane_id.to_string().to_uppercase(),
        );
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
                    u64::try_from(CHECKPOINT_STAGE_SEAL_PLAINTEXT_BYTES)
                        .expect("seal size")
                        + record_overhead,
                )
            })
            .expect("upload envelope");
        assert_eq!(CHECKPOINT_STAGE_MAX_FILES_PER_UPLOAD, 1_026);
        assert_eq!(CHECKPOINT_STAGE_MAX_BYTES_PER_UPLOAD, expected_upload_bytes);
        assert_eq!(expected_upload_bytes, 268_739_888);
        assert_eq!(
            policy.max_stage_files,
            policy.max_retained_uploads * CHECKPOINT_STAGE_MAX_FILES_PER_UPLOAD
        );
        assert_eq!(
            policy.max_stage_bytes,
            u64::try_from(policy.max_retained_uploads).expect("retention count")
                * CHECKPOINT_STAGE_MAX_BYTES_PER_UPLOAD
        );
    }

    #[test]
    fn checkpoint_stage_shared_logical_identities_are_content_stable_and_complete_only(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
            GuardianCheckpointCandidateIdentityV1::from_canonical_begin_plaintext(
                &begin_payload,
            )?;
        let exact_retry_identity =
            GuardianCheckpointCandidateIdentityV1::from_canonical_begin_plaintext(
                &begin_payload,
            )?;
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
    fn checkpoint_stage_begin_chunk_retry_and_gap_are_durable_and_exact(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
        assert_eq!(
            store.apply_begin(&begin)?,
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
        Ok(())
    }

    #[test]
    fn checkpoint_stage_torn_candidate_poison_is_retained_but_fresh_upload_progresses(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
    fn checkpoint_stage_conflicting_begin_cannot_relabel_the_durable_candidate(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
    fn checkpoint_stage_global_retention_cap_fails_closed_without_reclamation(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
    fn checkpoint_stage_invalid_utf8_prefixed_entry_is_never_ignored(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-checkpoint-raw-name-",
            OutputSegmentPolicy::production(),
        )?;
        let store = pipeline.checkpoint_stage_store();
        let mut raw_name = b"checkpoint-invalid-".to_vec();
        raw_name.push(0xff);
        let path = store
            .inner
            .directory_path
            .join(OsString::from_vec(raw_name));
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
    fn checkpoint_stage_torn_seal_is_quarantined_without_hiding_other_uploads(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
        let seal_path = checkpoint_seal_path(
            &store.inner,
            shape.key(),
            inspection.publication_id,
        )?;
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
    fn input_wal_reopen_is_withheld_without_anti_rollback_authority_and_stays_out_of_output_namespace(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-input-spawn-retry-",
            OutputSegmentPolicy::production(),
        )?;
        let guardian_incarnation = Uuid::from_u128(0x71);
        let pane_id = Uuid::from_u128(0x72);
        let path = input_journal_path(
            &pipeline.directory_path,
            guardian_incarnation,
            pane_id,
        );
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
    fn output_directory_creation_stays_with_pinned_parent_across_aba_restore(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-parent-aba-")?;
        let pinned = open_directory_no_follow(&directory)?;
        let retained_original = directory.with_file_name(format!(
            "ft-guardian-output-parent-aba-original-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&directory, &retained_original)?;
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(
            &directory,
            std::fs::Permissions::from_mode(0o700),
        )?;

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
    fn output_artifact_access_stays_with_pinned_directory_across_aba_restore(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let parent = kept_private_directory("ft-guardian-output-artifact-aba-")?;
        let output_directory = parent.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(
            &output_directory,
            std::fs::Permissions::from_mode(0o700),
        )?;
        let pinned = open_directory_no_follow(&output_directory)?;
        let retained_original = parent.join(format!(
            "{OUTPUT_DIRECTORY_NAME}-original-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&output_directory, &retained_original)?;
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(
            &output_directory,
            std::fs::Permissions::from_mode(0o700),
        )?;

        let artifact_path = output_directory.join("descriptor-bound-artifact");
        let mut artifact =
            create_private_file_new_at(&pinned, &output_directory, &artifact_path)?;
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
        assert!(create_private_file_new_at(
            &pinned,
            &output_directory,
            &parent.join("escaped-artifact"),
        )
        .is_err());

        let retained_replacement = parent.join(format!(
            "{OUTPUT_DIRECTORY_NAME}-replacement-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&output_directory, &retained_replacement)?;
        std::fs::rename(&retained_original, &output_directory)?;

        validate_file_identity_at(&pinned, &output_directory, &artifact_path, identity)?;
        assert!(read_directory_names(&pinned)?
            .iter()
            .any(|name| name.as_os_str() == OsStr::new("descriptor-bound-artifact")));
        assert_eq!(std::fs::read(&artifact_path)?, b"pinned-original");
        assert!(!retained_replacement
            .join("descriptor-bound-artifact")
            .exists());
        Ok(())
    }

    #[test]
    fn output_key_provisioning_resumes_partial_private_stage_without_partial_final_name(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-key-stage-")?;
        let output_directory = directory.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(
            &output_directory,
            std::fs::Permissions::from_mode(0o700),
        )?;
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
    fn partial_final_output_key_fails_closed_without_overwrite(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-key-final-cut-")?;
        let output_directory = directory.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(
            &output_directory,
            std::fs::Permissions::from_mode(0o700),
        )?;
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
    fn existing_output_key_is_never_loaded_through_a_replaced_directory_name(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-key-parent-swap-")?;
        let output_directory = directory.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(
            &output_directory,
            std::fs::Permissions::from_mode(0o700),
        )?;
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
        std::fs::set_permissions(
            &output_directory,
            std::fs::Permissions::from_mode(0o700),
        )?;
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
    fn missing_output_key_never_creates_a_split_authority_over_existing_artifacts(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let directory = kept_private_directory("ft-guardian-output-key-missing-")?;
        let output_directory = directory.join(OUTPUT_DIRECTORY_NAME);
        std::fs::create_dir(&output_directory)?;
        std::fs::set_permissions(
            &output_directory,
            std::fs::Permissions::from_mode(0o700),
        )?;
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
        assert!(!output_directory
            .join(format!("{OUTPUT_KEY_NAME}.provisioning"))
            .exists());
        assert_eq!(
            std::fs::read(&retained_ciphertext)?,
            b"retained encrypted crash evidence"
        );
        Ok(())
    }

    #[test]
    fn encrypted_commits_are_ordered_and_recoverable_only_after_sync_receipts(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
    fn rotation_cold_open_and_replay_preserve_exact_cross_segment_authority(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let policy = tiny_rotation_policy(4);
        let (directory, poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-rotation-",
            policy,
        )?;
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
                    prior.segment_identity.segment_id()
                        != segment.segment_identity.segment_id()
                }));
            }
            assert_eq!(recover_all_segment_bytes(&authority)?, b"abbccc");
        }
        drop(journal);
        drop(pipeline);
        drop(poll);

        let (_reopened_poll, reopened_pipeline) = reopen_pipeline(&directory, policy)?;
        let reopened = reopened_pipeline.cold_open_pane_for_validation(
            guardian_incarnation,
            pane_id,
        )?;
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
    fn log_byte_limit_rolls_over_before_the_frozen_append_seam_rejects_payload(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let policy = OutputSegmentPolicy {
            journal_limits: GuardianOutputJournalLimits {
                max_record_bytes: 64,
                max_log_bytes: 352,
                max_records: 10,
            },
            max_segments: 3,
            max_durable_pane_bytes: 4 * 1024,
        };
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-log-rollover-",
            policy,
        )?;
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
    fn torn_manifest_rolls_back_to_last_exact_chain_without_reclamation(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-manifest-cut-",
            tiny_rotation_policy(4),
        )?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        durable_commit(&pipeline, pane_id, &journal, b"one")?;
        durable_commit(&pipeline, pane_id, &journal, b"two")?;
        let torn = pipeline.publish_torn_manifest_candidate(
            guardian_incarnation,
            pane_id,
            3,
        )?;
        assert!(torn.exists());
        drop(journal);

        let reopened = pipeline.cold_open_pane_for_validation(
            guardian_incarnation,
            pane_id,
        )?;
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

        let validated = pipeline.cold_open_pane_for_validation(
            guardian_incarnation,
            pane_id,
        )?;
        let authority = validated
            .authority
            .lock()
            .map_err(|_| "validated journal authority was poisoned")?;
        assert_eq!(authority.manifest.snapshot.revision, 3);
        assert_eq!(authority.manifest_history.len(), 3);
        assert_eq!(authority.relevant_files, 10);
        assert_eq!(recover_all_segment_bytes(&authority)?, b"onetwothree");
        assert!(pipeline
            .relevant_pane_paths(guardian_incarnation, pane_id)?
            .contains(&torn));
        Ok(())
    }

    #[test]
    fn empty_published_spawn_preparation_is_idempotent_but_nonempty_retry_is_blocked(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-spawn-retry-",
            tiny_rotation_policy(3),
        )?;
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
            assert_eq!(authority.segments[0].segment_identity.segment_id(), segment_id);
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
        assert!(pipeline
            .prepare_pane(guardian_incarnation, pane_id)
            .is_err());
        assert_eq!(
            pipeline.relevant_pane_paths(guardian_incarnation, pane_id)?,
            initial_paths
        );
        Ok(())
    }

    #[test]
    fn path_link_change_fails_closed_before_plaintext_is_committed(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
    fn manifest_hardlink_is_rejected_on_cold_open_and_retained_as_evidence(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-manifest-link-",
            tiny_rotation_policy(3),
        )?;
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

        assert!(pipeline
            .cold_open_pane_for_validation(guardian_incarnation, pane_id)
            .is_err());
        assert_eq!(std::fs::metadata(&manifest_path)?.nlink(), 2);
        assert!(manifest_path.exists());
        assert!(evidence_link.exists());
        Ok(())
    }

    #[test]
    fn marked_manifest_checksum_corruption_fails_closed_instead_of_rolling_back(
    ) -> Result<(), Box<dyn std::error::Error>> {
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

        assert!(pipeline
            .cold_open_pane_for_validation(guardian_incarnation, pane_id)
            .is_err());
        assert_eq!(std::fs::metadata(&manifest_path)?.len(), manifest_bytes);
        assert!(manifest_path.exists());
        assert!(publication_path.exists());
        Ok(())
    }

    #[test]
    fn orphan_publication_marker_fails_closed_and_is_never_reclaimed(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
        let marker = create_private_file_new_at(
            &pipeline.directory,
            &pipeline.directory_path,
            &orphan,
        )?;
        marker.sync_all()?;
        pipeline.directory.sync_all()?;
        drop(marker);
        drop(journal);

        assert!(pipeline
            .cold_open_pane_for_validation(guardian_incarnation, pane_id)
            .is_err());
        assert!(orphan.exists());
        Ok(())
    }

    #[test]
    fn preexisting_symlink_cannot_capture_a_collision_resistant_segment_path(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-symlink-",
            tiny_rotation_policy(3),
        )?;
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

        assert!(create_segment_at_identity(
            &pipeline.directory,
            &pipeline.directory_path,
            guardian_incarnation,
            identity,
            pipeline.cipher.clone(),
            pipeline.policy.journal_limits,
        )
        .is_err());
        assert!(std::fs::symlink_metadata(&collision_path)?
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&target)?, b"unchanged");
        Ok(())
    }

    #[test]
    fn segment_count_exhaustion_fails_closed_without_creating_or_reclaiming_files(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-capacity-",
            tiny_rotation_policy(2),
        )?;
        let guardian_incarnation = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let journal = pipeline.prepare_pane(guardian_incarnation, pane_id)?;
        assert_eq!(durable_commit(&pipeline, pane_id, &journal, b"one")?.sequence(), 1);
        assert_eq!(durable_commit(&pipeline, pane_id, &journal, b"two")?.sequence(), 2);
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
    fn total_disk_byte_bound_stops_admission_before_an_extra_record_or_publication(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let policy = OutputSegmentPolicy {
            journal_limits: GuardianOutputJournalLimits {
                max_record_bytes: 64,
                max_log_bytes: 512,
                max_records: 10,
            },
            max_segments: 4,
            max_durable_pane_bytes: 620,
        };
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-disk-cap-",
            policy,
        )?;
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
            .try_submit(
                pane_id,
                journal.clone(),
                zeroizing_test_bytes(b"x"),
            )
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
    fn incomplete_segment_tail_is_rejected_without_truncation_or_reclamation(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, _poll, pipeline) = pipeline_with_policy(
            "ft-guardian-output-segment-cut-",
            tiny_rotation_policy(3),
        )?;
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

        assert!(pipeline
            .cold_open_pane_for_validation(guardian_incarnation, pane_id)
            .is_err());
        assert_eq!(std::fs::metadata(&segment_path)?.len(), torn_bytes);
        assert!(segment_path.exists());
        Ok(())
    }

    #[test]
    fn queue_admission_is_bounded_even_without_a_draining_worker(
    ) -> Result<(), Box<dyn std::error::Error>> {
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
        let OutputQueuePushError::Saturated(mut second) =
            queue.try_push(second).expect_err("full queue must reject atomically as saturated")
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
