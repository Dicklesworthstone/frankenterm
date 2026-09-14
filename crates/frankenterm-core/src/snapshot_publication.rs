//! Crash-safe on-disk layout and publication based on the FrankenFS generation pattern.
//!
//! # Architecture & Guarantees
//!
//! - **Dual Root Slots (`slot_a.root` / `slot_b.root`)**: Alternating root slots ensure that the sole
//!   verified root generation is never overwritten. New roots are written into the inactive slot only
//!   after the full generation independently verifies.
//! - **Descriptor-Bound Cross-Process Publication Lock**: An exclusive OS-level advisory lock on
//!   `.publication.lock` serializes publisher root inspection, CAS, verification, and slot replacement,
//!   as well as object checks and publishing. This prevents concurrent publishers from advancing
//!   slots twice and clobbering active or fallback roots.
//! - **In-Staging Candidate Verification**: Candidate envelopes and their full object closures are
//!   authenticated and validated via the caller's [`RootVerifier`] *in staging before* replacing the
//!   inactive slot selector. Bad proposals are rejected without touching or destroying valid fallback roots.
//! - **Decoupled Verification**: Readable files and SHA-256 checksums only yield a [`RootSlotCandidate`].
//!   Promotion to a verified root requires an independent caller-provided [`RootVerifier`] that
//!   authenticates signatures, checks object graph completeness, and verifies predecessor identity.
//! - **Predecessor Binding**: Root publication is explicitly predecessor-bound; publication fails closed
//!   if the active root does not match the expected predecessor generation and manifest digest.
//! - **Immutable Objects (`objects/<id>.obj`)**: Content-addressed recovery objects are published
//!   with no-clobber semantics under the publication lock. Existing identical objects are adopted idempotently;
//!   conflicting residue fails closed with an error.
//! - **No File Deletion**: Incomplete staging files or torn generation roots are strictly preserved on
//!   disk for post-incident forensics.
//! - **Fail-Closed Fallback**: If the newest root slot is torn or fails verifier inspection, readers
//!   seamlessly fall back to the intact predecessor root.
//! - **No-Follow & No-Chmod Root Admission**: Root and child directory leaves reject symlinks.
//!   Caller-selected ancestor paths use ambient directory resolution. Existing roots require owner-private
//!   permissions (0700); untrusted existing directories are never silently `chmod`'d.
//! - **Bounded Resource Allocation**: All directory entries, error records, root manifests, and objects
//!   are bounded before memory allocation.
//! - **Descriptor-Relative & Capability-Audited I/O**: Directory handles ([`cap_std::fs::Dir`]) with
//!   symlink-following disabled (`FollowSymlinks::No` / `O_NOFOLLOW`) bind leaf operations to opened directories.
//!   Advisory locks coordinate participating publishers; arbitrary same-user directory replacement is outside this contract.
//! - **Fsync Ordering**: File payloads are flushed (`sync_all`) before atomic rename, followed by directory sync.

#![forbid(unsafe_code)]

use std::fmt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
#[cfg(unix)]
use cap_std::fs::MetadataExt as _;
use cap_std::fs::{Dir, File, OpenOptions};
use fs2::FileExt as _;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cx::Cx;
use crate::snapshot_repair::{
    DEFAULT_SYMBOL_SIZE, ExpectedRecoveryIdentity, RepairError, RepairObjectChunk,
    RepairObjectDescriptor, RepairObjectLimits, RepairProtectionClass, encode_repair_object,
    shared_admission_controller,
};

// =============================================================================
// Constants & Limits
// =============================================================================

/// 16-byte magic header identifying a valid generation root envelope.
pub const ENVELOPE_MAGIC: &[u8; 16] = b"FTRECROOTv1\0\0\0\0\0";

/// Canonical filename for root slot A.
pub const ROOT_SLOT_A_NAME: &str = "slot_a.root";
/// Canonical filename for root slot B.
pub const ROOT_SLOT_B_NAME: &str = "slot_b.root";

/// Subdirectory name for immutable objects.
pub const OBJECTS_DIR_NAME: &str = "objects";
/// Subdirectory name for dual root slots.
pub const ROOTS_DIR_NAME: &str = "roots";
/// Subdirectory name for generation metadata.
pub const GENERATIONS_DIR_NAME: &str = "generations";
const DISCOVERY_SLOT_A: &str = "slot_a.discovery";
const DISCOVERY_SLOT_B: &str = "slot_b.discovery";
const DISCOVERY_DOMAIN: &[u8] = b"frankenterm.snapshot-recovery.committed-root-discovery.v1\0";
const MAX_DISCOVERY_BYTES: usize = 16 * 1024;

/// Staging file prefix for atomic publication.
pub const STAGE_PREFIX: &str = ".stage.";

/// Default ceiling on root manifest byte size (1 MiB).
pub const DEFAULT_MAX_ROOT_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Default ceiling on individual object byte size (64 MiB).
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;
/// Default ceiling on directory entries examined in a single scan.
pub const DEFAULT_MAX_DIR_ENTRIES: usize = 1024;
/// Default ceiling on diagnostic error records retained.
pub const DEFAULT_MAX_ERROR_RECORDS: usize = 64;

/// Maximum allowed byte size for the serialized envelope JSON header (64 KiB).
pub const MAX_ENVELOPE_HEADER_BYTES: u64 = 64 * 1024;

/// Total framing overhead for an envelope: magic (16) + header length (4) + header JSON (64 KiB) + trailer SHA-256 (32).
pub const MAX_ENVELOPE_OVERHEAD_BYTES: u64 = 16 + 4 + MAX_ENVELOPE_HEADER_BYTES + 32;

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

// =============================================================================
// Configuration & Limits
// =============================================================================

/// Resource bounds enforced during publication and read operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationLimits {
    /// Maximum allowed bytes for a root manifest envelope payload.
    pub max_root_manifest_bytes: u64,
    /// Maximum allowed bytes for an individual recovery object.
    pub max_object_bytes: u64,
    /// Maximum directory entries scanned in a single operation.
    pub max_dir_entries: usize,
    /// Maximum number of diagnostic error records collected.
    pub max_error_records: usize,
}

impl PublicationLimits {
    /// Maximum allowed bytes for the serialized on-disk root envelope,
    /// accounting for framing overhead (magic, headers, trailer checksum)
    /// plus the root manifest payload bound.
    #[must_use]
    pub fn max_root_envelope_bytes(&self) -> u64 {
        self.max_root_manifest_bytes
            .saturating_add(MAX_ENVELOPE_OVERHEAD_BYTES)
    }
}

impl Default for PublicationLimits {
    fn default() -> Self {
        Self {
            max_root_manifest_bytes: DEFAULT_MAX_ROOT_MANIFEST_BYTES,
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            max_dir_entries: DEFAULT_MAX_DIR_ENTRIES,
            max_error_records: DEFAULT_MAX_ERROR_RECORDS,
        }
    }
}

// =============================================================================
// Errors
// =============================================================================

/// Errors that can occur during recovery snapshot publication or selection.
#[derive(Debug, Error)]
pub enum PublicationError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Invalid object ID '{object_id}': {reason}")]
    InvalidObjectId { object_id: String, reason: String },

    #[error("Payload exceeds maximum permitted size of {max_bytes} bytes (found {actual_bytes})")]
    OversizedPayload { max_bytes: u64, actual_bytes: u64 },

    #[error("Directory scan exceeded maximum bound of {max_entries} entries")]
    ExcessiveDirectoryEntries { max_entries: usize },

    #[error(
        "Immutable object '{object_id}' already exists with conflicting SHA-256 (expected {expected}, found {existing})"
    )]
    ObjectConflict {
        object_id: String,
        expected: String,
        existing: String,
    },

    #[error("Corrupted or invalid root envelope in slot {slot:?}: {reason}")]
    InvalidEnvelope { slot: RootSlot, reason: String },

    #[error(
        "Predecessor mismatch for generation {generation}: expected gen {expected_gen} ({expected_hash}), active root was {actual:?}"
    )]
    PredecessorMismatch {
        generation: u64,
        expected_gen: u64,
        expected_hash: String,
        actual: Option<(u64, String)>,
    },

    #[error("Initial generation (gen {generation}) cannot specify a predecessor binding")]
    InitialGenerationWithPredecessor { generation: u64 },

    #[error("Subsequent generation (gen {generation}) requires an explicit predecessor binding")]
    MissingPredecessorBinding { generation: u64 },

    #[error("Non-monotonic generation number: candidate gen {candidate} <= active gen {active}")]
    NonMonotonicGeneration { candidate: u64, active: u64 },

    #[error("Target file {path} has invalid security attributes: {reason}")]
    InsecurePermissions { path: PathBuf, reason: String },

    #[error(
        "File length changed concurrently during read of {path}: expected {expected} bytes from metadata, read {actual} bytes"
    )]
    FileLengthChanged {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },

    #[error(
        "Independent caller verification rejected root generation {generation} in slot {slot:?}: {reason}"
    )]
    VerificationRejected {
        slot: RootSlot,
        generation: u64,
        reason: String,
    },

    #[error(
        "Generation {generation} is already published with conflicting content (expected {expected}, found {existing})"
    )]
    GenerationConflict {
        generation: u64,
        expected: String,
        existing: String,
    },

    #[error("Target file '{path}' already exists")]
    AlreadyExists { path: PathBuf },

    #[error("No verified root available")]
    NoVerifiedRoot,

    #[error("Publication lock is contended or changed repeatedly")]
    PublicationBusy,

    #[error("Recovery generation zero is reserved and cannot be published")]
    InvalidGeneration,

    #[error("Both verified root slots claim generation {generation}; root selection is ambiguous")]
    AmbiguousGeneration { generation: u64 },

    #[error("Repair storage failed: {0}")]
    Repair(#[from] RepairError),

    #[error("Repair discovery validation failed: {0}")]
    InvalidDiscovery(&'static str),

    #[error("Publication cancelled")]
    Cancelled,
}

impl PublicationError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

// =============================================================================
// Envelope & Identifiers
// =============================================================================

/// Identified root slot in dual-slot publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RootSlot {
    SlotA,
    SlotB,
}

impl RootSlot {
    /// Canonical filename in the `roots/` subdirectory.
    #[must_use]
    pub fn filename(self) -> &'static str {
        match self {
            Self::SlotA => ROOT_SLOT_A_NAME,
            Self::SlotB => ROOT_SLOT_B_NAME,
        }
    }

    /// Alternate inactive slot.
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::SlotA => Self::SlotB,
            Self::SlotB => Self::SlotA,
        }
    }
}

impl fmt::Display for RootSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SlotA => write!(f, "SlotA"),
            Self::SlotB => write!(f, "SlotB"),
        }
    }
}

/// Predecessor constraint required when publishing a new generation root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredecessorBinding {
    pub expected_generation: u64,
    pub expected_hash: String,
}

/// Metadata header stored inside the root envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationEnvelopeHeader {
    pub generation: u64,
    pub publisher_id: String,
    pub predecessor_generation: Option<u64>,
    pub predecessor_hash: Option<String>,
    pub manifest_sha256: String,
    pub manifest_len: u64,
    pub created_at_ms: u64,
}

/// Raw candidate root read from one of the dual slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSlotCandidate {
    pub slot: RootSlot,
    pub generation: u64,
    pub publisher_id: String,
    pub predecessor_generation: Option<u64>,
    pub predecessor_hash: Option<String>,
    pub manifest_sha256: String,
    pub manifest_bytes: Vec<u8>,
    pub file_len: u64,
    pub created_at_ms: u64,
}

/// Request to publish a new generation root manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationRootPublishRequest {
    pub generation: u64,
    pub publisher_id: String,
    pub predecessor: Option<PredecessorBinding>,
    pub manifest_bytes: Vec<u8>,
    pub created_at_ms: u64,
}

/// Receipt returned upon successful publication of a generation root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationPublicationReceipt {
    pub generation: u64,
    pub publisher_id: String,
    pub slot: RootSlot,
    pub sha256: String,
    pub byte_len: u64,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairDescriptorReference {
    pub object_id: String,
    pub sha256: String,
    pub byte_len: u64,
}

/// Authenticated discovery is published only after the corresponding ordinary
/// root commits. Its namespace and root identity must match caller-trusted values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootRepairDiscovery {
    pub namespace: String,
    pub root_object_id: [u8; 32],
    pub generation: u64,
    pub slot: RootSlot,
    pub outer_envelope_sha256: [u8; 32],
    pub outer_envelope_len: u64,
    pub predecessor: Option<PredecessorBinding>,
    pub repair: RepairDescriptorReference,
}

struct PreparedRootRepair<'a> {
    cx: &'a Cx,
    key: &'a [u8],
    namespace: &'a str,
    root_object_id: [u8; 32],
    envelope_sha256: [u8; 32],
    envelope_len: u64,
    descriptor: RepairDescriptorReference,
}

pub fn repair_descriptor_object_id(representation_id: &[u8; 32]) -> String {
    format!("repair-descriptor-{}", hex::encode(representation_id))
}

/// Descriptor for an immutable recovery object to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryObjectPayload {
    pub object_id: String,
    pub expected_sha256: String,
    pub ciphertext_bytes: Vec<u8>,
}

/// Receipt returned upon successful publication of an immutable object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPublicationReceipt {
    pub object_id: String,
    pub sha256: String,
    pub byte_len: u64,
    pub path: PathBuf,
    pub was_already_present: bool,
}

/// Diagnostic for a root slot candidate that was torn, corrupted, or rejected by verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TornRootDiagnostic {
    pub slot: RootSlot,
    pub generation: Option<u64>,
    pub reason: String,
}

/// Result of evaluating root slot candidates with an independent verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRootSelection<T> {
    /// Highest fully verified generation root.
    pub current: Option<T>,
    /// Predecessor verified generation root (last-known-good).
    pub previous: Option<T>,
    /// Candidates that were torn, corrupted, or failed verifier inspection.
    pub torn_or_rejected: Vec<TornRootDiagnostic>,
}

// =============================================================================
// Verifier Trait
// =============================================================================

/// Independent caller-provided verifier required to validate root authenticity.
///
/// Implementations must check cryptographic signatures, confirm that all required
/// objects referenced by the root manifest exist and validate, and verify that
/// the predecessor lineage is unbroken.
pub trait RootVerifier {
    type Error: std::error::Error + Send + Sync + 'static;
    type Verified: Send + Sync;

    /// Validates the root candidate.
    fn verify_root(
        &self,
        candidate: &RootSlotCandidate,
        store: &SnapshotPublicationStore,
    ) -> Result<Self::Verified, Self::Error>;
}

impl<F, T, E> RootVerifier for F
where
    F: Fn(&RootSlotCandidate, &SnapshotPublicationStore) -> Result<T, E>,
    E: std::error::Error + Send + Sync + 'static,
    T: Send + Sync,
{
    type Error = E;
    type Verified = T;

    fn verify_root(
        &self,
        candidate: &RootSlotCandidate,
        store: &SnapshotPublicationStore,
    ) -> Result<Self::Verified, Self::Error> {
        self(candidate, store)
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Compute hex-encoded SHA-256 digest.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Validate object ID format (alphanumeric, dashes, underscores, dots; no paths).
fn validate_object_id(id: &str) -> Result<(), PublicationError> {
    if id.is_empty() || id.len() > 255 {
        return Err(PublicationError::InvalidObjectId {
            object_id: id.to_string(),
            reason: "ID length must be between 1 and 255 bytes".to_string(),
        });
    }
    if id.starts_with('.') || id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(PublicationError::InvalidObjectId {
            object_id: id.to_string(),
            reason: "ID must not start with '.' or contain path separators or '..'".to_string(),
        });
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(PublicationError::InvalidObjectId {
            object_id: id.to_string(),
            reason: "ID contains invalid characters (allowed: ASCII alphanumeric, '-', '_', '.')"
                .to_string(),
        });
    }
    Ok(())
}

/// Check security attributes on opened file handle.
fn check_opened_file_security(file: &File, path: &Path) -> Result<u64, PublicationError> {
    let metadata = file.metadata().map_err(|e| PublicationError::io(path, e))?;

    if !metadata.is_file() {
        return Err(PublicationError::InsecurePermissions {
            path: path.to_path_buf(),
            reason: "target is not a regular file".to_string(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(PublicationError::InsecurePermissions {
                path: path.to_path_buf(),
                reason: format!("file mode {mode:#o} is not private (expected 0600)"),
            });
        }
        let current_uid = rustix::process::geteuid().as_raw();
        if metadata.uid() != current_uid {
            return Err(PublicationError::InsecurePermissions {
                path: path.to_path_buf(),
                reason: format!(
                    "file owner UID {} does not match current process UID {current_uid}",
                    metadata.uid()
                ),
            });
        }
        if metadata.nlink() != 1 {
            return Err(PublicationError::InsecurePermissions {
                path: path.to_path_buf(),
                reason: format!(
                    "file hard link count {} is greater than 1",
                    metadata.nlink()
                ),
            });
        }
    }

    Ok(metadata.len())
}

/// Generate unique stage filename.
fn generate_stage_name(sha256_prefix: &str) -> String {
    let pid = std::process::id();
    let nonce = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{STAGE_PREFIX}{pid}.{nonce}.{sha256_prefix}.tmp")
}

/// RAII guard representing an exclusive, descriptor-bound cross-process publication lock.
pub struct PublicationLock {
    file: std::fs::File,
}

impl Drop for PublicationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Verify that a directory has private permissions (0700) and is owned by the current process.
/// Does NOT mutate or chmod the directory.
fn verify_directory_security(dir: &Dir, path: &Path) -> Result<(), PublicationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        let std_file = dir
            .try_clone()
            .map_err(|e| PublicationError::io(path, e))?
            .into_std_file();
        let metadata = std_file
            .metadata()
            .map_err(|e| PublicationError::io(path, e))?;

        if !metadata.is_dir() {
            return Err(PublicationError::InsecurePermissions {
                path: path.to_path_buf(),
                reason: "path is not a directory".to_string(),
            });
        }

        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(PublicationError::InsecurePermissions {
                path: path.to_path_buf(),
                reason: format!(
                    "directory permissions {mode:#o} are too broad (group/other permissions must be 0)"
                ),
            });
        }

        let current_uid = rustix::process::geteuid().as_raw();
        if metadata.uid() != current_uid {
            return Err(PublicationError::InsecurePermissions {
                path: path.to_path_buf(),
                reason: format!(
                    "directory owner UID {} does not match current process UID {current_uid}",
                    metadata.uid()
                ),
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, path);
    }
    Ok(())
}

/// Creates missing ancestor directories while rejecting symlinks at inspected leaves.
/// Existing ancestor prefixes are resolved through ambient directory access.
fn ensure_directory_hierarchy_nofollow(path: &Path) -> Result<Dir, PublicationError> {
    if path.as_os_str().is_empty() || path == Path::new(".") {
        return Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .map_err(|e| PublicationError::io(path, e));
    }

    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(PublicationError::InsecurePermissions {
                    path: path.to_path_buf(),
                    reason: "directory component is a symlink (symlinks forbidden)".to_string(),
                });
            }
            let directory = Dir::open_ambient_dir(path, cap_std::ambient_authority())
                .map_err(|e| PublicationError::io(path, e))?;
            // A preceding mkdir may be visible even though its parent sync
            // failed. Reopening that directory must complete the entry sync.
            if let Some(parent) = path.parent() {
                let parent_path = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                let parent_directory =
                    Dir::open_ambient_dir(parent_path, cap_std::ambient_authority())
                        .map_err(|error| PublicationError::io(parent_path, error))?;
                sync_directory(&parent_directory, parent_path)?;
            }
            Ok(directory)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let parent_dir = ensure_directory_hierarchy_nofollow(parent)?;
            let leaf = path
                .file_name()
                .ok_or_else(|| PublicationError::InsecurePermissions {
                    path: path.to_path_buf(),
                    reason: "path has no leaf component".to_string(),
                })?;
            let mut builder = cap_std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use cap_std::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            parent_dir
                .create_dir_with(leaf, &builder)
                .map_err(|e| PublicationError::io(path, e))?;
            sync_directory(&parent_dir, parent)?;
            parent_dir
                .open_dir_nofollow(leaf)
                .map_err(|e| PublicationError::io(path, e))
        }
        Err(e) => Err(PublicationError::io(path, e)),
    }
}

/// Persist directory entry changes, including a rename whose earlier caller
/// lost its reply or received an error from the final directory sync.
fn sync_directory(directory: &Dir, path: &Path) -> Result<(), PublicationError> {
    #[cfg(test)]
    if DIRECTORY_SYNC_FAILURE.with(|failure| {
        let mut failure = failure.borrow_mut();
        if failure.as_deref() == Some(path) {
            failure.take();
            true
        } else {
            false
        }
    }) {
        return Err(PublicationError::io(
            path,
            std::io::Error::other("injected directory sync failure"),
        ));
    }
    directory
        .open(".")
        .and_then(|file| file.sync_all())
        .map_err(|error| PublicationError::io(path, error))
}

#[cfg(test)]
thread_local! {
    /// One-shot fault at the actual durability boundary; isolated per test thread.
    static DIRECTORY_SYNC_FAILURE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn sync_adopted_file(file: &File, parent: &Dir, path: &Path) -> Result<(), PublicationError> {
    file.sync_all()
        .map_err(|error| PublicationError::io(path, error))?;
    sync_directory(parent, path.parent().unwrap_or_else(|| Path::new(".")))
}

// =============================================================================
// Bounded File Reading Guarding Against File Growth
// =============================================================================

/// Reads a file with bounds enforcement guarding against concurrent file growth.
///
/// Limits read to `max_bytes + 1` via `Read::take`, preventing unbounded allocation.
/// Verifies that the total bytes read does not exceed `max_bytes` and exactly matches `expected_len`.
fn read_file_bounded_exact<R: Read>(
    reader: &mut R,
    expected_len: u64,
    max_bytes: u64,
    path: &Path,
) -> Result<Vec<u8>, PublicationError> {
    if expected_len > max_bytes {
        return Err(PublicationError::OversizedPayload {
            max_bytes,
            actual_bytes: expected_len,
        });
    }

    let read_limit = max_bytes.saturating_add(1);
    let mut limited = reader.take(read_limit);
    let mut buffer = Vec::with_capacity(expected_len as usize);
    limited
        .read_to_end(&mut buffer)
        .map_err(|e| PublicationError::io(path, e))?;

    let actual_len = buffer.len() as u64;
    if actual_len > max_bytes {
        return Err(PublicationError::OversizedPayload {
            max_bytes,
            actual_bytes: actual_len,
        });
    }
    if actual_len != expected_len {
        return Err(PublicationError::FileLengthChanged {
            path: path.to_path_buf(),
            expected: expected_len,
            actual: actual_len,
        });
    }

    Ok(buffer)
}

// =============================================================================
// Envelope Encoding / Decoding
// =============================================================================

/// Encode a root manifest into a framed, checksummed binary envelope.
fn encode_root_envelope(
    request: &GenerationRootPublishRequest,
    manifest_sha256: &str,
) -> Result<Vec<u8>, PublicationError> {
    let header = GenerationEnvelopeHeader {
        generation: request.generation,
        publisher_id: request.publisher_id.clone(),
        predecessor_generation: request.predecessor.as_ref().map(|p| p.expected_generation),
        predecessor_hash: request
            .predecessor
            .as_ref()
            .map(|p| p.expected_hash.clone()),
        manifest_sha256: manifest_sha256.to_string(),
        manifest_len: request.manifest_bytes.len() as u64,
        created_at_ms: request.created_at_ms,
    };
    encode_root_parts(&header, &request.manifest_bytes)
}

fn encode_root_parts(
    header: &GenerationEnvelopeHeader,
    manifest_bytes: &[u8],
) -> Result<Vec<u8>, PublicationError> {
    let header_json = serde_json::to_vec(header).map_err(|e| {
        PublicationError::InvalidEnvelope {
            slot: RootSlot::SlotA, // context set by caller
            reason: format!("failed to serialize envelope header: {e}"),
        }
    })?;

    if (header_json.len() as u64) > MAX_ENVELOPE_HEADER_BYTES {
        return Err(PublicationError::OversizedPayload {
            max_bytes: MAX_ENVELOPE_HEADER_BYTES,
            actual_bytes: header_json.len() as u64,
        });
    }

    let header_len =
        u32::try_from(header_json.len()).map_err(|_| PublicationError::OversizedPayload {
            max_bytes: u32::MAX as u64,
            actual_bytes: header_json.len() as u64,
        })?;

    let total_capacity = ENVELOPE_MAGIC.len() + 4 + header_json.len() + manifest_bytes.len() + 32; // 32 bytes for SHA-256 trailer

    let mut buffer = Vec::with_capacity(total_capacity);
    buffer.extend_from_slice(ENVELOPE_MAGIC);
    buffer.extend_from_slice(&header_len.to_le_bytes());
    buffer.extend_from_slice(&header_json);
    buffer.extend_from_slice(manifest_bytes);

    // Compute trailer checksum over all preceding bytes
    let trailer_hash = Sha256::digest(&buffer);
    buffer.extend_from_slice(&trailer_hash);

    Ok(buffer)
}

/// Decode and validate a root envelope read from a slot.
fn decode_root_envelope(
    slot: RootSlot,
    bytes: &[u8],
    max_envelope_bytes: u64,
    max_manifest_bytes: u64,
) -> Result<RootSlotCandidate, PublicationError> {
    if (bytes.len() as u64) > max_envelope_bytes {
        return Err(PublicationError::OversizedPayload {
            max_bytes: max_envelope_bytes,
            actual_bytes: bytes.len() as u64,
        });
    }

    let min_envelope_len = ENVELOPE_MAGIC.len() + 4 + 32;
    if bytes.len() < min_envelope_len {
        return Err(PublicationError::InvalidEnvelope {
            slot,
            reason: format!(
                "envelope is too short: {} bytes (minimum required: {min_envelope_len})",
                bytes.len()
            ),
        });
    }

    if &bytes[..ENVELOPE_MAGIC.len()] != ENVELOPE_MAGIC {
        return Err(PublicationError::InvalidEnvelope {
            slot,
            reason: "invalid magic header".to_string(),
        });
    }

    let content_len = bytes.len() - 32;
    let expected_trailer = &bytes[content_len..];
    let actual_trailer = Sha256::digest(&bytes[..content_len]);
    if expected_trailer != actual_trailer.as_slice() {
        return Err(PublicationError::InvalidEnvelope {
            slot,
            reason: "trailer SHA-256 checksum mismatch (corrupted or torn payload)".to_string(),
        });
    }

    let mut header_len_bytes = [0u8; 4];
    header_len_bytes.copy_from_slice(&bytes[ENVELOPE_MAGIC.len()..ENVELOPE_MAGIC.len() + 4]);
    let header_len = u32::from_le_bytes(header_len_bytes) as usize;

    if (header_len as u64) > MAX_ENVELOPE_HEADER_BYTES {
        return Err(PublicationError::InvalidEnvelope {
            slot,
            reason: format!(
                "envelope header length {header_len} exceeds maximum allowed header size of {MAX_ENVELOPE_HEADER_BYTES}"
            ),
        });
    }

    let header_start = ENVELOPE_MAGIC.len() + 4;
    let header_end = header_start + header_len;
    if header_end > content_len {
        return Err(PublicationError::InvalidEnvelope {
            slot,
            reason: "envelope header length extends past payload boundary".to_string(),
        });
    }

    let header: GenerationEnvelopeHeader = serde_json::from_slice(&bytes[header_start..header_end])
        .map_err(|e| PublicationError::InvalidEnvelope {
            slot,
            reason: format!("malformed envelope header JSON: {e}"),
        })?;

    let manifest_bytes = bytes[header_end..content_len].to_vec();
    if (manifest_bytes.len() as u64) != header.manifest_len {
        return Err(PublicationError::InvalidEnvelope {
            slot,
            reason: format!(
                "manifest length mismatch: header says {} bytes, found {}",
                header.manifest_len,
                manifest_bytes.len()
            ),
        });
    }

    if (manifest_bytes.len() as u64) > max_manifest_bytes {
        return Err(PublicationError::OversizedPayload {
            max_bytes: max_manifest_bytes,
            actual_bytes: manifest_bytes.len() as u64,
        });
    }

    let actual_manifest_hash = sha256_hex(&manifest_bytes);
    if actual_manifest_hash != header.manifest_sha256 {
        return Err(PublicationError::InvalidEnvelope {
            slot,
            reason: format!(
                "manifest content SHA-256 mismatch: header says {}, actual is {actual_manifest_hash}",
                header.manifest_sha256
            ),
        });
    }

    Ok(RootSlotCandidate {
        slot,
        generation: header.generation,
        publisher_id: header.publisher_id,
        predecessor_generation: header.predecessor_generation,
        predecessor_hash: header.predecessor_hash,
        manifest_sha256: header.manifest_sha256,
        manifest_bytes,
        file_len: bytes.len() as u64,
        created_at_ms: header.created_at_ms,
    })
}

// =============================================================================
// Publication Store
// =============================================================================

/// Descriptor-relative store managing immutable object and root publication.
pub struct SnapshotPublicationStore {
    root_path: PathBuf,
    root_dir: Dir,
    objects_dir: Dir,
    roots_dir: Dir,
    #[allow(dead_code)]
    generations_dir: Dir,
    limits: PublicationLimits,
}

impl SnapshotPublicationStore {
    /// Opens or initializes a snapshot publication store at the designated root path.
    ///
    /// # Security & Symlink Invariants
    /// - Rejects any symlinks in the path hierarchy without following them.
    /// - If the root directory exists, verifies owner-private permissions (0700) and UID;
    ///   does NOT chmod untrusted existing directories.
    /// - If creating new directories, creates them with 0700 permissions by construction.
    pub fn open(
        root_path: impl Into<PathBuf>,
        limits: PublicationLimits,
    ) -> Result<Self, PublicationError> {
        let root_path = root_path.into();

        if root_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(PublicationError::InsecurePermissions {
                path: root_path.clone(),
                reason: "root path contains parent directory component ('..')".to_string(),
            });
        }

        // Check symlink_metadata on root_path
        let root_exists = match std::fs::symlink_metadata(&root_path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(PublicationError::InsecurePermissions {
                        path: root_path.clone(),
                        reason: "root path is a symlink (symlinks are forbidden)".to_string(),
                    });
                }
                if !meta.is_dir() {
                    return Err(PublicationError::InsecurePermissions {
                        path: root_path.clone(),
                        reason: "root path exists but is not a directory".to_string(),
                    });
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(PublicationError::io(&root_path, e)),
        };

        let root_dir = if root_exists {
            // Existing directory: open and verify security WITHOUT mutating permissions (no chmod)
            let dir = Dir::open_ambient_dir(&root_path, cap_std::ambient_authority())
                .map_err(|e| PublicationError::io(&root_path, e))?;
            verify_directory_security(&dir, &root_path)?;
            dir
        } else {
            // New directory: create parent hierarchy if needed and create leaf with 0700
            let parent_path = root_path.parent().unwrap_or_else(|| Path::new("."));
            let leaf =
                root_path
                    .file_name()
                    .ok_or_else(|| PublicationError::InsecurePermissions {
                        path: root_path.clone(),
                        reason: "root path has no leaf name".to_string(),
                    })?;

            // Ensure parent directory exists without symlinks
            let parent_dir = ensure_directory_hierarchy_nofollow(parent_path)?;

            let mut builder = cap_std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use cap_std::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            parent_dir
                .create_dir_with(leaf, &builder)
                .map_err(|e| PublicationError::io(&root_path, e))?;

            let dir = parent_dir
                .open_dir_nofollow(leaf)
                .map_err(|e| PublicationError::io(&root_path, e))?;
            verify_directory_security(&dir, &root_path)?;
            dir
        };

        // Complete the root's parent-entry durability on both creation and a
        // retry that finds the directory from an earlier incomplete open.
        if let Some(parent) = root_path.parent() {
            let parent_path = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            let parent_directory = ensure_directory_hierarchy_nofollow(parent_path)?;
            sync_directory(&parent_directory, parent_path)?;
        }

        let objects_dir = Self::ensure_private_child_dir(&root_dir, OBJECTS_DIR_NAME, &root_path)?;
        let roots_dir = Self::ensure_private_child_dir(&root_dir, ROOTS_DIR_NAME, &root_path)?;
        let generations_dir =
            Self::ensure_private_child_dir(&root_dir, GENERATIONS_DIR_NAME, &root_path)?;

        Ok(Self {
            root_path,
            root_dir,
            objects_dir,
            roots_dir,
            generations_dir,
            limits,
        })
    }

    /// Root filesystem path of the publication store.
    #[must_use]
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Configured resource limits.
    #[must_use]
    pub fn limits(&self) -> &PublicationLimits {
        &self.limits
    }

    fn ensure_private_child_dir(
        parent: &Dir,
        name: &str,
        base_path: &Path,
    ) -> Result<Dir, PublicationError> {
        let child_path = base_path.join(name);

        let exists = match parent.symlink_metadata(name) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(PublicationError::InsecurePermissions {
                        path: child_path,
                        reason: format!(
                            "child directory '{name}' is a symlink (symlinks forbidden)"
                        ),
                    });
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(PublicationError::io(&child_path, e)),
        };

        if exists {
            let child_dir = parent
                .open_dir_nofollow(name)
                .map_err(|e| PublicationError::io(&child_path, e))?;
            verify_directory_security(&child_dir, &child_path)?;
            sync_directory(parent, base_path)?;
            Ok(child_dir)
        } else {
            let mut builder = cap_std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use cap_std::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            parent
                .create_dir_with(name, &builder)
                .map_err(|e| PublicationError::io(&child_path, e))?;
            let child_dir = parent
                .open_dir_nofollow(name)
                .map_err(|e| PublicationError::io(&child_path, e))?;
            verify_directory_security(&child_dir, &child_path)?;

            // Sync parent directory after creating child
            sync_directory(parent, base_path)?;

            Ok(child_dir)
        }
    }

    /// Acquires an exclusive, descriptor-bound cross-process publication lock on `.publication.lock`.
    ///
    /// Revalidates the named lock inode after nonblocking acquisition. Contention
    /// and repeated inode changes return immediately to the caller for retry.
    pub fn acquire_publication_lock(&self) -> Result<PublicationLock, PublicationError> {
        let lock_leaf = ".publication.lock";
        let lock_path = self.root_path.join(lock_leaf);

        let mut opts = OpenOptions::new();
        opts.read(true)
            .write(true)
            .create(true)
            .follow(FollowSymlinks::No);

        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }

        for _ in 0..4 {
            let cap_file = self
                .root_dir
                .open_with(lock_leaf, &opts)
                .map_err(|e| PublicationError::io(&lock_path, e))?;

            check_opened_file_security(&cap_file, &lock_path)?;

            let std_file = cap_file.into_std();
            fs2::FileExt::try_lock_exclusive(&std_file).map_err(|e| {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    PublicationError::PublicationBusy
                } else {
                    PublicationError::io(&lock_path, e)
                }
            })?;

            // Revalidate named lock inode after nonblocking acquisition.
            let opened_meta = std_file
                .metadata()
                .map_err(|e| PublicationError::io(&lock_path, e))?;

            let named_meta = match self.root_dir.symlink_metadata(lock_leaf) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    drop(std_file);
                    continue;
                }
                Err(e) => return Err(PublicationError::io(&lock_path, e)),
            };

            if named_meta.file_type().is_symlink() || !named_meta.is_file() {
                return Err(PublicationError::InsecurePermissions {
                    path: lock_path.clone(),
                    reason: "named lock path is not a regular file or is a symlink".to_string(),
                });
            }

            #[cfg(unix)]
            {
                use cap_std::fs::MetadataExt as _;
                use std::os::unix::fs::MetadataExt as _;
                if opened_meta.dev() != named_meta.dev() || opened_meta.ino() != named_meta.ino() {
                    // The name changed during acquisition; close the old locked descriptor and retry.
                    drop(std_file);
                    continue;
                }
            }

            return Ok(PublicationLock { file: std_file });
        }
        Err(PublicationError::PublicationBusy)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn publish_object_noreplace(
        objects_dir: &Dir,
        stage_name: &str,
        target_name: &str,
    ) -> Result<(), PublicationError> {
        use rustix::fs::{RenameFlags, renameat_with};

        let parent_file = objects_dir
            .open(".")
            .map_err(|e| PublicationError::io(target_name, e))?
            .into_std();

        match renameat_with(
            &parent_file,
            stage_name,
            &parent_file,
            target_name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => Ok(()),
            Err(error) if error == rustix::io::Errno::EXIST => {
                Err(PublicationError::AlreadyExists {
                    path: PathBuf::from(target_name),
                })
            }
            Err(error) => Err(PublicationError::io(
                target_name,
                std::io::Error::from_raw_os_error(error.raw_os_error()),
            )),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn publish_object_noreplace(
        _objects_dir: &Dir,
        _stage_name: &str,
        target_name: &str,
    ) -> Result<(), PublicationError> {
        Err(PublicationError::io(
            target_name,
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "atomic no-replace publication is unavailable on this platform",
            ),
        ))
    }

    // -------------------------------------------------------------------------
    // Object Storage (Immutable, No-Clobber)
    // -------------------------------------------------------------------------

    fn checkpoint_publication(cx: &Cx) -> Result<(), PublicationError> {
        cx.checkpoint().map_err(|_| PublicationError::Cancelled)
    }

    fn prepare_persisted_repair(
        &self,
        cx: &Cx,
        envelope: &[u8],
        semantic_id: [u8; 32],
        generation: u64,
        key: &[u8],
        protection: RepairProtectionClass,
    ) -> Result<RepairDescriptorReference, PublicationError> {
        Self::checkpoint_publication(cx)?;
        let limits = RepairObjectLimits::default();
        let descriptor = encode_repair_object(
            cx,
            envelope,
            semantic_id,
            generation,
            key,
            DEFAULT_SYMBOL_SIZE,
            protection,
            shared_admission_controller(),
            limits,
            |_index, _manifest, records| {
                cx.checkpoint().map_err(|_| RepairError::Cancelled)?;
                let digest = sha256_hex(records);
                let object_id = format!("repair-records-{digest}");
                self.publish_object(&RecoveryObjectPayload {
                    object_id: object_id.clone(),
                    expected_sha256: digest,
                    ciphertext_bytes: records.to_vec(),
                })
                .map_err(|error| RepairError::Storage(error.to_string()))?;
                Ok(object_id)
            },
        )?;
        let bytes = descriptor.to_authenticated_bytes(key, limits)?;
        Self::checkpoint_publication(cx)?;
        let representation_id: [u8; 32] = Sha256::digest(envelope).into();
        let receipt = self.publish_object(&RecoveryObjectPayload {
            object_id: repair_descriptor_object_id(&representation_id),
            expected_sha256: sha256_hex(&bytes),
            ciphertext_bytes: bytes,
        })?;
        Ok(RepairDescriptorReference {
            object_id: receipt.object_id,
            sha256: receipt.sha256,
            byte_len: receipt.byte_len,
        })
    }

    /// Persist a complete authenticated repair closure before publishing its
    /// envelope. All records and the descriptor use immutable no-clobber writes.
    pub fn publish_repair_protected_object(
        &self,
        cx: &Cx,
        object: &RecoveryObjectPayload,
        semantic_id: [u8; 32],
        generation: u64,
        key: &[u8],
    ) -> Result<ObjectPublicationReceipt, PublicationError> {
        Self::checkpoint_publication(cx)?;
        validate_object_id(&object.object_id)?;
        if generation == 0 {
            return Err(PublicationError::InvalidGeneration);
        }
        if object.ciphertext_bytes.len() as u64 > self.limits.max_object_bytes {
            return Err(PublicationError::OversizedPayload {
                max_bytes: self.limits.max_object_bytes,
                actual_bytes: object.ciphertext_bytes.len() as u64,
            });
        }
        if sha256_hex(&object.ciphertext_bytes) != object.expected_sha256 {
            return Err(PublicationError::InvalidDiscovery(
                "object digest mismatch before repair publication",
            ));
        }
        self.prepare_persisted_repair(
            cx,
            &object.ciphertext_bytes,
            semantic_id,
            generation,
            key,
            RepairProtectionClass::High,
        )?;
        Self::checkpoint_publication(cx)?;
        self.publish_object(object)
    }

    pub fn publish_repair_protected_generation_root<V: RootVerifier>(
        &self,
        cx: &Cx,
        request: &GenerationRootPublishRequest,
        namespace: &str,
        root_object_id: [u8; 32],
        key: &[u8],
        verifier: &V,
    ) -> Result<GenerationPublicationReceipt, PublicationError> {
        Self::checkpoint_publication(cx)?;
        if request.generation == 0 {
            return Err(PublicationError::InvalidGeneration);
        }
        if namespace.is_empty() || namespace.len() > 1024 || key.is_empty() {
            return Err(PublicationError::InvalidDiscovery(
                "invalid trusted namespace or key",
            ));
        }
        if request.manifest_bytes.len() as u64 > self.limits.max_root_manifest_bytes {
            return Err(PublicationError::OversizedPayload {
                max_bytes: self.limits.max_root_manifest_bytes,
                actual_bytes: request.manifest_bytes.len() as u64,
            });
        }
        let envelope = encode_root_envelope(request, &sha256_hex(&request.manifest_bytes))?;
        if envelope.len() as u64 > self.limits.max_root_envelope_bytes() {
            return Err(PublicationError::OversizedPayload {
                max_bytes: self.limits.max_root_envelope_bytes(),
                actual_bytes: envelope.len() as u64,
            });
        }
        let descriptor = self.prepare_persisted_repair(
            cx,
            &envelope,
            root_object_id,
            request.generation,
            key,
            RepairProtectionClass::Maximum,
        )?;
        let protection = PreparedRootRepair {
            cx,
            key,
            namespace,
            root_object_id,
            envelope_sha256: Sha256::digest(&envelope).into(),
            envelope_len: envelope.len() as u64,
            descriptor,
        };
        Self::checkpoint_publication(cx)?;
        self.publish_generation_root_inner(request, verifier, Some(&protection))
    }

    pub(crate) fn read_object_bounded(
        &self,
        object_id: &str,
        bound: u64,
    ) -> Result<Vec<u8>, PublicationError> {
        validate_object_id(object_id)?;
        let filename = format!("{object_id}.obj");
        let path = self.root_path.join(OBJECTS_DIR_NAME).join(&filename);
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = self
            .objects_dir
            .open_with(&filename, &options)
            .map_err(|error| PublicationError::io(&path, error))?;
        let len = check_opened_file_security(&file, &path)?;
        read_file_bounded_exact(
            &mut file,
            len,
            bound.min(self.limits.max_object_bytes),
            &path,
        )
    }

    pub fn read_repair_descriptor(
        &self,
        expected: &ExpectedRecoveryIdentity,
        key: &[u8],
    ) -> Result<RepairObjectDescriptor, PublicationError> {
        let limits = RepairObjectLimits::default();
        let bytes = self.read_object_bounded(
            &repair_descriptor_object_id(&expected.representation_id),
            limits.max_descriptor_bytes as u64,
        )?;
        Ok(RepairObjectDescriptor::from_authenticated_bytes(
            &bytes, expected, key, limits,
        )?)
    }

    pub fn read_repair_descriptor_bytes(
        &self,
        reference: &RepairDescriptorReference,
    ) -> Result<Vec<u8>, PublicationError> {
        if reference.byte_len > RepairObjectLimits::default().max_descriptor_bytes as u64 {
            return Err(PublicationError::InvalidDiscovery(
                "descriptor exceeds limit",
            ));
        }
        let bytes = self.read_object_bounded(&reference.object_id, reference.byte_len)?;
        if bytes.len() as u64 != reference.byte_len || sha256_hex(&bytes) != reference.sha256 {
            return Err(PublicationError::InvalidDiscovery(
                "descriptor reference mismatch",
            ));
        }
        Ok(bytes)
    }

    pub fn read_repair_records(
        &self,
        chunk: &RepairObjectChunk,
    ) -> Result<Vec<u8>, PublicationError> {
        self.read_object_bounded(&chunk.records_object_id, chunk.record_bytes_len()? as u64)
    }

    pub fn decode_recovered_root(
        &self,
        slot: RootSlot,
        bytes: &[u8],
    ) -> Result<RootSlotCandidate, PublicationError> {
        decode_root_envelope(
            slot,
            bytes,
            self.limits.max_root_envelope_bytes(),
            self.limits.max_root_manifest_bytes,
        )
    }

    pub fn root_candidate_matches_discovery(
        &self,
        candidate: &RootSlotCandidate,
        discovery: &RootRepairDiscovery,
    ) -> Result<bool, PublicationError> {
        let predecessor = candidate
            .predecessor_generation
            .map(|generation| PredecessorBinding {
                expected_generation: generation,
                expected_hash: candidate.predecessor_hash.clone().unwrap_or_default(),
            });
        if candidate.slot != discovery.slot
            || candidate.generation != discovery.generation
            || predecessor != discovery.predecessor
            || candidate.file_len != discovery.outer_envelope_len
            || candidate.manifest_bytes.len() as u64 > self.limits.max_root_manifest_bytes
        {
            return Ok(false);
        }
        let header = GenerationEnvelopeHeader {
            generation: candidate.generation,
            publisher_id: candidate.publisher_id.clone(),
            predecessor_generation: candidate.predecessor_generation,
            predecessor_hash: candidate.predecessor_hash.clone(),
            manifest_sha256: candidate.manifest_sha256.clone(),
            manifest_len: candidate.manifest_bytes.len() as u64,
            created_at_ms: candidate.created_at_ms,
        };
        let bytes = encode_root_parts(&header, &candidate.manifest_bytes)?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        Ok(bytes.len() as u64 == discovery.outer_envelope_len
            && digest == discovery.outer_envelope_sha256)
    }

    /// Publishes an immutable recovery object into `objects/<object_id>.obj`.
    ///
    /// # Invariants
    /// - Checks whether `<object_id>.obj` already exists.
    /// - If existing file matches exact SHA-256 and byte length, adopts it idempotently (`was_already_present = true`).
    /// - If existing file has conflicting contents, fails closed with [`PublicationError::ObjectConflict`].
    /// - Never overwrites or deletes existing files.
    /// - Stages write to a private temporary file, calls `sync_all()`, then atomic renames and syncs directory.
    pub fn publish_object(
        &self,
        object: &RecoveryObjectPayload,
    ) -> Result<ObjectPublicationReceipt, PublicationError> {
        validate_object_id(&object.object_id)?;

        let payload_len = object.ciphertext_bytes.len() as u64;
        if payload_len > self.limits.max_object_bytes {
            return Err(PublicationError::OversizedPayload {
                max_bytes: self.limits.max_object_bytes,
                actual_bytes: payload_len,
            });
        }

        let computed_sha256 = sha256_hex(&object.ciphertext_bytes);
        if computed_sha256 != object.expected_sha256 {
            return Err(PublicationError::ObjectConflict {
                object_id: object.object_id.clone(),
                expected: object.expected_sha256.clone(),
                existing: computed_sha256,
            });
        }

        let target_name = format!("{}.obj", object.object_id);
        let target_full_path = self.root_path.join(OBJECTS_DIR_NAME).join(&target_name);

        // Acquire descriptor-bound cross-process publication lock covering check, stage, and rename
        let _publication_lock = self.acquire_publication_lock()?;

        // Check if object already exists
        let mut open_opts = OpenOptions::new();
        open_opts.read(true).follow(FollowSymlinks::No);
        match self.objects_dir.open_with(&target_name, &open_opts) {
            Ok(existing_file) => {
                let actual_len = check_opened_file_security(&existing_file, &target_full_path)?;
                if actual_len != payload_len {
                    return Err(PublicationError::ObjectConflict {
                        object_id: object.object_id.clone(),
                        expected: format!("{computed_sha256} ({payload_len} bytes)"),
                        existing: format!("conflicting length ({actual_len} bytes)"),
                    });
                }
                let mut reader = existing_file;
                let existing_bytes = read_file_bounded_exact(
                    &mut reader,
                    actual_len,
                    self.limits.max_object_bytes,
                    &target_full_path,
                )?;
                let existing_sha256 = sha256_hex(&existing_bytes);
                if existing_sha256 != computed_sha256 {
                    return Err(PublicationError::ObjectConflict {
                        object_id: object.object_id.clone(),
                        expected: computed_sha256,
                        existing: existing_sha256,
                    });
                }
                sync_adopted_file(&reader, &self.objects_dir, &target_full_path)?;
                return Ok(ObjectPublicationReceipt {
                    object_id: object.object_id.clone(),
                    sha256: computed_sha256,
                    byte_len: payload_len,
                    path: target_full_path,
                    was_already_present: true,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(PublicationError::io(&target_full_path, e)),
        }

        // Object does not exist. Write to a new staging file.
        let stage_name = generate_stage_name(&computed_sha256[..8]);
        let stage_path = self.root_path.join(OBJECTS_DIR_NAME).join(&stage_name);

        let mut stage_opts = OpenOptions::new();
        stage_opts
            .read(true)
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);

        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            stage_opts.mode(0o600);
        }

        let mut stage_file = self
            .objects_dir
            .open_with(&stage_name, &stage_opts)
            .map_err(|e| PublicationError::io(&stage_path, e))?;

        stage_file
            .write_all(&object.ciphertext_bytes)
            .map_err(|e| PublicationError::io(&stage_path, e))?;

        stage_file
            .sync_all()
            .map_err(|e| PublicationError::io(&stage_path, e))?;

        // Reread and verify stage descriptor bytes before publication
        let stage_len = check_opened_file_security(&stage_file, &stage_path)?;
        if stage_len != payload_len {
            return Err(PublicationError::FileLengthChanged {
                path: stage_path.clone(),
                expected: payload_len,
                actual: stage_len,
            });
        }
        stage_file
            .seek(SeekFrom::Start(0))
            .map_err(|e| PublicationError::io(&stage_path, e))?;
        let read_bytes = read_file_bounded_exact(
            &mut stage_file,
            stage_len,
            self.limits.max_object_bytes,
            &stage_path,
        )?;
        if read_bytes != object.ciphertext_bytes {
            return Err(PublicationError::ObjectConflict {
                object_id: object.object_id.clone(),
                expected: computed_sha256,
                existing: sha256_hex(&read_bytes),
            });
        }

        // Revalidate stage binding before atomic publication
        let stage_named_meta = self
            .objects_dir
            .symlink_metadata(&stage_name)
            .map_err(|e| PublicationError::io(&stage_path, e))?;
        let stage_fd_meta = stage_file
            .metadata()
            .map_err(|e| PublicationError::io(&stage_path, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if stage_named_meta.dev() != stage_fd_meta.dev()
                || stage_named_meta.ino() != stage_fd_meta.ino()
            {
                return Err(PublicationError::InsecurePermissions {
                    path: stage_path.clone(),
                    reason: "staging object file inode changed before publish".to_string(),
                });
            }
        }
        drop(stage_file);

        // Atomic no-clobber publication + exact-existing adoption
        match Self::publish_object_noreplace(&self.objects_dir, &stage_name, &target_name) {
            Ok(()) => {
                // Fsync parent objects directory
                sync_directory(&self.objects_dir, &self.root_path.join(OBJECTS_DIR_NAME))?;

                Ok(ObjectPublicationReceipt {
                    object_id: object.object_id.clone(),
                    sha256: computed_sha256,
                    byte_len: payload_len,
                    path: target_full_path,
                    was_already_present: false,
                })
            }
            Err(PublicationError::AlreadyExists { .. }) => {
                // Target already exists: no-clobber primitive prevented overwrite.
                // Adopt exact existing target if content matches.
                let existing_file = self
                    .objects_dir
                    .open_with(&target_name, &open_opts)
                    .map_err(|e| PublicationError::io(&target_full_path, e))?;
                let actual_len = check_opened_file_security(&existing_file, &target_full_path)?;
                let mut reader = existing_file;
                let existing_bytes = read_file_bounded_exact(
                    &mut reader,
                    actual_len,
                    self.limits.max_object_bytes,
                    &target_full_path,
                )?;
                let existing_sha256 = sha256_hex(&existing_bytes);
                if existing_sha256 == computed_sha256 && actual_len == payload_len {
                    sync_adopted_file(&reader, &self.objects_dir, &target_full_path)?;
                    Ok(ObjectPublicationReceipt {
                        object_id: object.object_id.clone(),
                        sha256: computed_sha256,
                        byte_len: payload_len,
                        path: target_full_path,
                        was_already_present: true,
                    })
                } else {
                    Err(PublicationError::ObjectConflict {
                        object_id: object.object_id.clone(),
                        expected: computed_sha256,
                        existing: existing_sha256,
                    })
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Reads an immutable recovery object by its unique ID.
    pub fn read_object(&self, object_id: &str) -> Result<Vec<u8>, PublicationError> {
        validate_object_id(object_id)?;
        let target_name = format!("{object_id}.obj");
        let target_path = self.root_path.join(OBJECTS_DIR_NAME).join(&target_name);

        let mut opts = OpenOptions::new();
        opts.read(true).follow(FollowSymlinks::No);

        let file = self
            .objects_dir
            .open_with(&target_name, &opts)
            .map_err(|e| PublicationError::io(&target_path, e))?;

        let file_len = check_opened_file_security(&file, &target_path)?;
        let mut reader = file;
        let bytes = read_file_bounded_exact(
            &mut reader,
            file_len,
            self.limits.max_object_bytes,
            &target_path,
        )?;

        Ok(bytes)
    }

    /// Checks whether an immutable recovery object exists and has valid permissions.
    pub fn has_object(&self, object_id: &str) -> Result<bool, PublicationError> {
        validate_object_id(object_id)?;
        let target_name = format!("{object_id}.obj");
        let target_path = self.root_path.join(OBJECTS_DIR_NAME).join(&target_name);

        let mut opts = OpenOptions::new();
        opts.read(true).follow(FollowSymlinks::No);

        match self.objects_dir.open_with(&target_name, &opts) {
            Ok(file) => {
                check_opened_file_security(&file, &target_path)?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(PublicationError::io(&target_path, e)),
        }
    }

    /// Lists all immutable object IDs present in the store, bounded by `max_dir_entries`.
    pub fn list_object_ids(&self) -> Result<Vec<String>, PublicationError> {
        let mut object_ids = Vec::new();
        let entries = self
            .objects_dir
            .entries()
            .map_err(|e| PublicationError::io(self.root_path.join(OBJECTS_DIR_NAME), e))?;

        for (entry_index, entry) in entries.enumerate() {
            if entry_index >= self.limits.max_dir_entries {
                return Err(PublicationError::ExcessiveDirectoryEntries {
                    max_entries: self.limits.max_dir_entries,
                });
            }
            let entry = entry
                .map_err(|e| PublicationError::io(self.root_path.join(OBJECTS_DIR_NAME), e))?;
            let name = entry.file_name();
            let name_str = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if let Some(obj_id) = name_str.strip_suffix(".obj") {
                if !name_str.starts_with('.') && validate_object_id(obj_id).is_ok() {
                    object_ids.push(obj_id.to_string());
                }
            }
        }
        object_ids.sort();
        Ok(object_ids)
    }

    // -------------------------------------------------------------------------
    // Root Candidate Inspection
    // -------------------------------------------------------------------------

    /// Inspects dual root slots and returns all readable candidate envelopes without validation.
    pub fn inspect_root_candidates(
        &self,
    ) -> Result<(Vec<RootSlotCandidate>, Vec<TornRootDiagnostic>), PublicationError> {
        let mut candidates = Vec::new();
        let mut diagnostics = Vec::new();

        for slot in [RootSlot::SlotA, RootSlot::SlotB] {
            let slot_filename = slot.filename();
            let slot_path = self.root_path.join(ROOTS_DIR_NAME).join(slot_filename);

            let mut opts = OpenOptions::new();
            opts.read(true).follow(FollowSymlinks::No);

            let file_result = self.roots_dir.open_with(slot_filename, &opts);
            match file_result {
                Ok(mut file) => {
                    let security_check = check_opened_file_security(&file, &slot_path);
                    let file_len = match security_check {
                        Ok(len) => len,
                        Err(e) => {
                            diagnostics.push(TornRootDiagnostic {
                                slot,
                                generation: None,
                                reason: format!("security validation failed: {e}"),
                            });
                            continue;
                        }
                    };

                    let max_envelope_bytes = self.limits.max_root_envelope_bytes();
                    if file_len > max_envelope_bytes {
                        diagnostics.push(TornRootDiagnostic {
                            slot,
                            generation: None,
                            reason: format!(
                                "slot file length {file_len} exceeds envelope limit of {max_envelope_bytes}"
                            ),
                        });
                        continue;
                    }

                    let bytes = match read_file_bounded_exact(
                        &mut file,
                        file_len,
                        max_envelope_bytes,
                        &slot_path,
                    ) {
                        Ok(b) => b,
                        Err(e) => {
                            diagnostics.push(TornRootDiagnostic {
                                slot,
                                generation: None,
                                reason: format!("failed to read slot file: {e}"),
                            });
                            continue;
                        }
                    };

                    match decode_root_envelope(
                        slot,
                        &bytes,
                        max_envelope_bytes,
                        self.limits.max_root_manifest_bytes,
                    ) {
                        Ok(candidate) => candidates.push(candidate),
                        Err(e) => diagnostics.push(TornRootDiagnostic {
                            slot,
                            generation: None,
                            reason: format!("corrupt envelope: {e}"),
                        }),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Empty slot is normal and expected on initial runs
                }
                Err(e) => {
                    diagnostics.push(TornRootDiagnostic {
                        slot,
                        generation: None,
                        reason: format!("failed to open root slot: {e}"),
                    });
                }
            }
        }

        Ok((candidates, diagnostics))
    }

    // -------------------------------------------------------------------------
    // Dual Root Selection with Caller Verifier
    // -------------------------------------------------------------------------

    /// Evaluates candidate root slots using an independent caller-provided [`RootVerifier`].
    ///
    /// Returns the highest verified generation as `current` and the preceding verified generation
    /// as `previous`. If the newest slot is torn or fails verification, the store automatically
    /// falls back to the intact predecessor root.
    pub fn select_verified_roots<V: RootVerifier>(
        &self,
        verifier: &V,
    ) -> Result<VerifiedRootSelection<V::Verified>, PublicationError> {
        let (candidates, mut diagnostics) = self.inspect_root_candidates()?;

        let mut verified_entries: Vec<(u64, RootSlot, V::Verified)> = Vec::new();

        for candidate in &candidates {
            match verifier.verify_root(candidate, self) {
                Ok(verified) => {
                    verified_entries.push((candidate.generation, candidate.slot, verified));
                }
                Err(e) => {
                    diagnostics.push(TornRootDiagnostic {
                        slot: candidate.slot,
                        generation: Some(candidate.generation),
                        reason: format!("independent verifier rejected candidate: {e}"),
                    });
                }
            }
        }

        // Sort descending by generation
        verified_entries.sort_by(|a, b| b.0.cmp(&a.0));

        if verified_entries.len() == 2 && verified_entries[0].0 == verified_entries[1].0 {
            return Err(PublicationError::AmbiguousGeneration {
                generation: verified_entries[0].0,
            });
        }
        let mut entries = verified_entries.into_iter();
        let current = entries.next().map(|entry| entry.2);
        let previous = entries.next().map(|entry| entry.2);

        if diagnostics.len() > self.limits.max_error_records {
            diagnostics.truncate(self.limits.max_error_records);
        }

        Ok(VerifiedRootSelection {
            current,
            previous,
            torn_or_rejected: diagnostics,
        })
    }

    // -------------------------------------------------------------------------
    // Predecessor-Bound Root Publication
    // -------------------------------------------------------------------------

    /// Publishes a new generation root manifest into the inactive root slot.
    ///
    /// # Safety & Preconditions
    /// 1. Validates that `request.generation` is strictly monotonic.
    /// 2. If $N > 1$, requires an explicit [`PredecessorBinding`] and validates that the currently
    ///    active verified root exactly matches the expected predecessor generation and manifest digest.
    /// 3. Writes candidate into the inactive slot (protecting the active root from in-place corruption).
    /// 4. Flushes the staging file (`sync_all`), renames into slot, and flushes directory.
    /// 5. Validates the newly published root in-process via the caller's verifier before reporting success.
    pub fn publish_generation_root<V: RootVerifier>(
        &self,
        request: &GenerationRootPublishRequest,
        verifier: &V,
    ) -> Result<GenerationPublicationReceipt, PublicationError> {
        self.publish_generation_root_inner(request, verifier, None)
    }

    fn publish_generation_root_inner<V: RootVerifier>(
        &self,
        request: &GenerationRootPublishRequest,
        verifier: &V,
        protection: Option<&PreparedRootRepair<'_>>,
    ) -> Result<GenerationPublicationReceipt, PublicationError> {
        if request.generation == 0 {
            return Err(PublicationError::InvalidGeneration);
        }
        let manifest_len = request.manifest_bytes.len() as u64;
        if manifest_len > self.limits.max_root_manifest_bytes {
            return Err(PublicationError::OversizedPayload {
                max_bytes: self.limits.max_root_manifest_bytes,
                actual_bytes: manifest_len,
            });
        }

        // 1. Acquire descriptor-bound cross-process publication lock covering inspect/CAS/verify/commit
        let _publication_lock = self.acquire_publication_lock()?;

        // 2. Re-inspect existing candidates and determine verified active root
        //    (inside publication lock to avoid races with concurrent publishers)
        let (mut candidates, _) = self.inspect_root_candidates()?;
        if let Some(protection) = protection {
            let (discoveries, _) = self.inspect_repair_discovery(
                protection.namespace,
                &protection.root_object_id,
                protection.key,
            )?;
            for discovery in discoveries {
                Self::checkpoint_publication(protection.cx)?;
                let ordinary = candidates.iter().find(|c| c.slot == discovery.slot);
                if let Some(candidate) = ordinary {
                    if self.root_candidate_matches_discovery(candidate, &discovery)?
                        && verifier.verify_root(candidate, self).is_ok()
                    {
                        continue;
                    }
                }
                let expected = ExpectedRecoveryIdentity::new(
                    discovery.outer_envelope_sha256,
                    protection.root_object_id,
                    discovery.generation,
                );
                let limits = RepairObjectLimits {
                    max_envelope_bytes: usize::try_from(self.limits.max_root_envelope_bytes())
                        .unwrap_or(usize::MAX)
                        .min(RepairObjectLimits::default().max_envelope_bytes),
                    ..RepairObjectLimits::default()
                };
                let descriptor_bytes = self.read_repair_descriptor_bytes(&discovery.repair)?;
                let descriptor = RepairObjectDescriptor::from_authenticated_bytes(
                    &descriptor_bytes,
                    &expected,
                    protection.key,
                    limits,
                )?;
                if descriptor.payload_len != discovery.outer_envelope_len {
                    return Err(PublicationError::InvalidDiscovery(
                        "committed repair length mismatch",
                    ));
                }
                let repaired = crate::snapshot_repair::decode_repair_object(
                    protection.cx,
                    &descriptor,
                    &expected,
                    protection.key,
                    shared_admission_controller(),
                    limits,
                    |chunk| {
                        self.read_repair_records(chunk)
                            .map_err(|error| RepairError::Storage(error.to_string()))
                    },
                )?;
                let recovered =
                    self.decode_recovered_root(discovery.slot, &repaired.reconstructed_envelope)?;
                if !self.root_candidate_matches_discovery(&recovered, &discovery)?
                    || verifier.verify_root(&recovered, self).is_err()
                {
                    return Err(PublicationError::InvalidDiscovery(
                        "committed repair graph rejected",
                    ));
                }
                // A newer ordinary root can be the durable commit of a publication
                // whose discovery write failed. Otherwise the authenticated
                // committed root is the authority for CAS and inactive-slot choice.
                let retain_newer = ordinary.is_some_and(|candidate| {
                    candidate.generation > recovered.generation
                        && verifier.verify_root(candidate, self).is_ok()
                });
                if !retain_newer {
                    candidates.retain(|candidate| candidate.slot != discovery.slot);
                    candidates.push(recovered);
                }
            }
            Self::checkpoint_publication(protection.cx)?;
        }

        let mut verified_candidates = Vec::new();
        for candidate in &candidates {
            if verifier.verify_root(candidate, self).is_ok() {
                verified_candidates.push(candidate);
            }
        }
        verified_candidates.sort_by(|a, b| b.generation.cmp(&a.generation));
        if verified_candidates.len() == 2
            && verified_candidates[0].generation == verified_candidates[1].generation
        {
            return Err(PublicationError::AmbiguousGeneration {
                generation: verified_candidates[0].generation,
            });
        }
        let active_candidate = verified_candidates.first().copied();

        let manifest_sha256 = sha256_hex(&request.manifest_bytes);

        // 3. Lost-reply retry reconciliation:
        // If ANY verified root in the store has the same generation as the request,
        // check for exact identity match. If identical: reconcile idempotently BEFORE
        // stale predecessor rejection!
        for candidate in &verified_candidates {
            if candidate.generation == request.generation {
                let expected_pred_gen = request.predecessor.as_ref().map(|p| p.expected_generation);
                let expected_pred_hash = request
                    .predecessor
                    .as_ref()
                    .map(|p| p.expected_hash.as_str());
                let actual_pred_hash = candidate.predecessor_hash.as_deref();

                if candidate.manifest_sha256 == manifest_sha256
                    && candidate.publisher_id == request.publisher_id
                    && candidate.predecessor_generation == expected_pred_gen
                    && actual_pred_hash == expected_pred_hash
                    && candidate.manifest_bytes == request.manifest_bytes
                    && candidate.created_at_ms == request.created_at_ms
                {
                    let candidate_path = self
                        .root_path
                        .join(ROOTS_DIR_NAME)
                        .join(candidate.slot.filename());
                    let mut options = OpenOptions::new();
                    options.read(true).follow(FollowSymlinks::No);
                    let candidate_file = self
                        .roots_dir
                        .open_with(candidate.slot.filename(), &options)
                        .map_err(|error| PublicationError::io(&candidate_path, error))?;
                    check_opened_file_security(&candidate_file, &candidate_path)?;
                    sync_adopted_file(&candidate_file, &self.roots_dir, &candidate_path)?;
                    if let Some(protection) = protection {
                        self.publish_committed_discovery(request, candidate.slot, protection)?;
                    }
                    return Ok(GenerationPublicationReceipt {
                        generation: request.generation,
                        publisher_id: request.publisher_id.clone(),
                        slot: candidate.slot,
                        sha256: manifest_sha256,
                        byte_len: candidate.file_len,
                        path: self
                            .root_path
                            .join(ROOTS_DIR_NAME)
                            .join(candidate.slot.filename()),
                    });
                } else {
                    return Err(PublicationError::GenerationConflict {
                        generation: request.generation,
                        expected: format!(
                            "publisher={}, hash={manifest_sha256}",
                            request.publisher_id
                        ),
                        existing: format!(
                            "publisher={}, hash={}",
                            candidate.publisher_id, candidate.manifest_sha256
                        ),
                    });
                }
            }
        }

        // 4. Validate generation ordering and predecessor linkage
        if request.generation <= 1 {
            if request.predecessor.is_some() {
                return Err(PublicationError::InitialGenerationWithPredecessor {
                    generation: request.generation,
                });
            }
            if let Some(active) = active_candidate {
                return Err(PublicationError::NonMonotonicGeneration {
                    candidate: request.generation,
                    active: active.generation,
                });
            }
        } else {
            let Some(binding) = &request.predecessor else {
                return Err(PublicationError::MissingPredecessorBinding {
                    generation: request.generation,
                });
            };

            let actual_active_tuple =
                active_candidate.map(|c| (c.generation, c.manifest_sha256.clone()));

            match active_candidate {
                Some(active) => {
                    if active.generation != binding.expected_generation
                        || active.manifest_sha256 != binding.expected_hash
                    {
                        return Err(PublicationError::PredecessorMismatch {
                            generation: request.generation,
                            expected_gen: binding.expected_generation,
                            expected_hash: binding.expected_hash.clone(),
                            actual: actual_active_tuple,
                        });
                    }
                    if request.generation <= active.generation {
                        return Err(PublicationError::NonMonotonicGeneration {
                            candidate: request.generation,
                            active: active.generation,
                        });
                    }
                }
                None => {
                    return Err(PublicationError::PredecessorMismatch {
                        generation: request.generation,
                        expected_gen: binding.expected_generation,
                        expected_hash: binding.expected_hash.clone(),
                        actual: None,
                    });
                }
            }
        }

        // 5. Select the INACTIVE slot to publish to
        let target_slot = match active_candidate {
            Some(active) => active.slot.other(),
            None => RootSlot::SlotA,
        };

        let envelope_bytes = encode_root_envelope(request, &manifest_sha256)?;
        let max_envelope_bytes = self.limits.max_root_envelope_bytes();
        if (envelope_bytes.len() as u64) > max_envelope_bytes {
            return Err(PublicationError::OversizedPayload {
                max_bytes: max_envelope_bytes,
                actual_bytes: envelope_bytes.len() as u64,
            });
        }

        // 6. Stage file in roots directory
        let stage_name = generate_stage_name(&manifest_sha256[..8]);
        let stage_path = self.root_path.join(ROOTS_DIR_NAME).join(&stage_name);

        let mut stage_opts = OpenOptions::new();
        stage_opts
            .read(true)
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);

        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            stage_opts.mode(0o600);
        }

        let mut stage_file = self
            .roots_dir
            .open_with(&stage_name, &stage_opts)
            .map_err(|e| PublicationError::io(&stage_path, e))?;

        stage_file
            .write_all(&envelope_bytes)
            .map_err(|e| PublicationError::io(&stage_path, e))?;

        stage_file
            .sync_all()
            .map_err(|e| PublicationError::io(&stage_path, e))?;

        // 7. Reread and decode bounded retained stage descriptor, verify actual candidate
        let file_len = check_opened_file_security(&stage_file, &stage_path)?;
        if file_len != envelope_bytes.len() as u64 {
            return Err(PublicationError::FileLengthChanged {
                path: stage_path.clone(),
                expected: envelope_bytes.len() as u64,
                actual: file_len,
            });
        }

        stage_file
            .seek(SeekFrom::Start(0))
            .map_err(|e| PublicationError::io(&stage_path, e))?;

        let actual_envelope_bytes =
            read_file_bounded_exact(&mut stage_file, file_len, max_envelope_bytes, &stage_path)?;

        let actual_candidate = decode_root_envelope(
            target_slot,
            &actual_envelope_bytes,
            max_envelope_bytes,
            self.limits.max_root_manifest_bytes,
        )?;

        // Identity check: actual candidate read from descriptor must match request
        if actual_candidate.generation != request.generation
            || actual_candidate.publisher_id != request.publisher_id
            || actual_candidate.manifest_sha256 != manifest_sha256
            || actual_candidate.manifest_bytes != request.manifest_bytes
            || actual_candidate.created_at_ms != request.created_at_ms
            || actual_candidate.predecessor_generation
                != request.predecessor.as_ref().map(|p| p.expected_generation)
            || actual_candidate.predecessor_hash
                != request
                    .predecessor
                    .as_ref()
                    .map(|p| p.expected_hash.clone())
        {
            return Err(PublicationError::InvalidEnvelope {
                slot: target_slot,
                reason: "stage descriptor read-back does not match requested envelope parameters"
                    .to_string(),
            });
        }

        // Verify actual candidate read from disk
        if let Err(e) = verifier.verify_root(&actual_candidate, self) {
            return Err(PublicationError::VerificationRejected {
                slot: target_slot,
                generation: request.generation,
                reason: format!(
                    "actual candidate failed caller verification in staging (both slots preserved): {e}"
                ),
            });
        }

        // 8. Revalidate stage binding before atomic rename
        let stage_named_meta = self
            .roots_dir
            .symlink_metadata(&stage_name)
            .map_err(|e| PublicationError::io(&stage_path, e))?;
        let stage_fd_meta = stage_file
            .metadata()
            .map_err(|e| PublicationError::io(&stage_path, e))?;

        if stage_named_meta.file_type().is_symlink() || !stage_named_meta.is_file() {
            return Err(PublicationError::InsecurePermissions {
                path: stage_path.clone(),
                reason: "stage file on disk is not a regular file or is a symlink".to_string(),
            });
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if stage_named_meta.dev() != stage_fd_meta.dev()
                || stage_named_meta.ino() != stage_fd_meta.ino()
            {
                return Err(PublicationError::InsecurePermissions {
                    path: stage_path.clone(),
                    reason: "stage file inode changed before rename (stage file was replaced)"
                        .to_string(),
                });
            }
        }

        // 9. Atomic rename stage file into target slot
        let target_filename = target_slot.filename();
        let target_full_path = self.root_path.join(ROOTS_DIR_NAME).join(target_filename);

        if let Some(protection) = protection {
            Self::checkpoint_publication(protection.cx)?;
        }

        self.roots_dir
            .rename(&stage_name, &self.roots_dir, target_filename)
            .map_err(|e| PublicationError::io(&target_full_path, e))?;

        // Revalidate target binding after rename
        let target_meta = self
            .roots_dir
            .symlink_metadata(target_filename)
            .map_err(|e| PublicationError::io(&target_full_path, e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if target_meta.dev() != stage_fd_meta.dev() || target_meta.ino() != stage_fd_meta.ino()
            {
                return Err(PublicationError::InsecurePermissions {
                    path: target_full_path.clone(),
                    reason: "target slot inode does not match verified staging inode after rename"
                        .to_string(),
                });
            }
        }

        // 10. Fsync roots parent directory
        sync_directory(&self.roots_dir, &self.root_path.join(ROOTS_DIR_NAME))?;

        drop(stage_file);

        if let Some(protection) = protection {
            self.publish_committed_discovery(request, target_slot, protection)?;
        }

        Ok(GenerationPublicationReceipt {
            generation: actual_candidate.generation,
            publisher_id: actual_candidate.publisher_id,
            slot: target_slot,
            sha256: manifest_sha256,
            byte_len: actual_candidate.file_len,
            path: target_full_path,
        })
    }

    fn discovery_name(slot: RootSlot) -> &'static str {
        match slot {
            RootSlot::SlotA => DISCOVERY_SLOT_A,
            RootSlot::SlotB => DISCOVERY_SLOT_B,
        }
    }

    fn decode_discovery(
        &self,
        slot: RootSlot,
        bytes: &[u8],
        namespace: &str,
        root_object_id: &[u8; 32],
        key: &[u8],
    ) -> Result<RootRepairDiscovery, PublicationError> {
        if key.is_empty() || bytes.len() < 32 || bytes.len() > MAX_DISCOVERY_BYTES {
            return Err(PublicationError::InvalidDiscovery(
                "invalid discovery framing or key",
            ));
        }
        let (payload, tag) = bytes.split_at(bytes.len() - 32);
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key)
            .map_err(|_| PublicationError::InvalidDiscovery("invalid discovery key"))?;
        mac.update(DISCOVERY_DOMAIN);
        mac.update(payload);
        mac.verify_slice(tag)
            .map_err(|_| PublicationError::InvalidDiscovery("discovery authentication failed"))?;
        let discovery: RootRepairDiscovery = serde_json::from_slice(payload).map_err(|_| {
            PublicationError::InvalidDiscovery("invalid authenticated discovery JSON")
        })?;
        if discovery.namespace != namespace
            || &discovery.root_object_id != root_object_id
            || discovery.slot != slot
        {
            return Err(PublicationError::InvalidDiscovery(
                "discovery trusted identity mismatch",
            ));
        }
        if discovery.generation == 0
            || discovery.outer_envelope_len == 0
            || discovery.outer_envelope_len > self.limits.max_root_envelope_bytes()
            || discovery.repair.byte_len == 0
            || discovery.repair.byte_len > RepairObjectLimits::default().max_descriptor_bytes as u64
            || discovery.repair.object_id
                != repair_descriptor_object_id(&discovery.outer_envelope_sha256)
            || discovery.repair.sha256.len() != 64
            || hex::decode(&discovery.repair.sha256).is_err()
        {
            return Err(PublicationError::InvalidDiscovery(
                "invalid discovery geometry or descriptor binding",
            ));
        }
        match &discovery.predecessor {
            None if discovery.generation == 1 => {}
            Some(predecessor)
                if predecessor.expected_generation > 0
                    && predecessor.expected_generation < discovery.generation
                    && predecessor.expected_hash.len() == 64
                    && hex::decode(&predecessor.expected_hash).is_ok() => {}
            _ => {
                return Err(PublicationError::InvalidDiscovery(
                    "invalid discovery predecessor",
                ));
            }
        }
        Ok(discovery)
    }

    /// Read only the two fixed authenticated discovery slots. A damaged or
    /// wrong-key slot becomes a diagnostic while an independently valid older
    /// slot remains usable; directory enumeration never supplies authority.
    pub fn inspect_repair_discovery(
        &self,
        namespace: &str,
        root_object_id: &[u8; 32],
        key: &[u8],
    ) -> Result<(Vec<RootRepairDiscovery>, Vec<TornRootDiagnostic>), PublicationError> {
        if namespace.is_empty() || namespace.len() > 1024 || key.is_empty() {
            return Err(PublicationError::InvalidDiscovery(
                "invalid trusted discovery identity or key",
            ));
        }
        let mut discoveries = Vec::new();
        let mut diagnostics = Vec::new();
        for slot in [RootSlot::SlotA, RootSlot::SlotB] {
            let name = Self::discovery_name(slot);
            let path = self.root_path.join(GENERATIONS_DIR_NAME).join(name);
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let result = match self.generations_dir.open_with(name, &options) {
                Ok(mut file) => check_opened_file_security(&file, &path)
                    .and_then(|len| {
                        read_file_bounded_exact(&mut file, len, MAX_DISCOVERY_BYTES as u64, &path)
                    })
                    .and_then(|bytes| {
                        self.decode_discovery(slot, &bytes, namespace, root_object_id, key)
                    }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => Err(PublicationError::io(&path, error)),
            };
            match result {
                Ok(discovery) => discoveries.push(discovery),
                Err(error) => diagnostics.push(TornRootDiagnostic {
                    slot,
                    generation: None,
                    reason: error.to_string(),
                }),
            }
        }
        discoveries.sort_by(|a, b| b.generation.cmp(&a.generation));
        if discoveries.len() == 2 && discoveries[0].generation == discoveries[1].generation {
            return Err(PublicationError::AmbiguousGeneration {
                generation: discoveries[0].generation,
            });
        }
        diagnostics.truncate(self.limits.max_error_records);
        Ok((discoveries, diagnostics))
    }

    /// Called only while the root publication lock is held and after its
    /// ordinary root has passed verification, rename and directory sync.
    fn publish_committed_discovery(
        &self,
        request: &GenerationRootPublishRequest,
        slot: RootSlot,
        protection: &PreparedRootRepair<'_>,
    ) -> Result<(), PublicationError> {
        Self::checkpoint_publication(protection.cx)?;
        // Bind discovery to the exact committed outer bytes, including framing
        // and predecessor header, rather than merely an equal generation.
        let committed_path = self.root_path.join(ROOTS_DIR_NAME).join(slot.filename());
        let mut committed_options = OpenOptions::new();
        committed_options.read(true).follow(FollowSymlinks::No);
        let mut committed_file = self
            .roots_dir
            .open_with(slot.filename(), &committed_options)
            .map_err(|error| PublicationError::io(&committed_path, error))?;
        let committed_len = check_opened_file_security(&committed_file, &committed_path)?;
        let committed_bytes = read_file_bounded_exact(
            &mut committed_file,
            committed_len,
            self.limits.max_root_envelope_bytes(),
            &committed_path,
        )?;
        let committed_digest: [u8; 32] = Sha256::digest(&committed_bytes).into();
        if committed_len != protection.envelope_len
            || committed_digest != protection.envelope_sha256
        {
            return Err(PublicationError::InvalidDiscovery(
                "committed outer root differs from repair representation",
            ));
        }
        let discovery = RootRepairDiscovery {
            namespace: protection.namespace.to_owned(),
            root_object_id: protection.root_object_id,
            generation: request.generation,
            slot,
            outer_envelope_sha256: protection.envelope_sha256,
            outer_envelope_len: protection.envelope_len,
            predecessor: request.predecessor.clone(),
            repair: protection.descriptor.clone(),
        };
        let mut bytes = serde_json::to_vec(&discovery)
            .map_err(|_| PublicationError::InvalidDiscovery("cannot encode discovery"))?;
        if bytes.len() > MAX_DISCOVERY_BYTES - 32 {
            return Err(PublicationError::InvalidDiscovery(
                "discovery exceeds limit",
            ));
        }
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(protection.key)
            .map_err(|_| PublicationError::InvalidDiscovery("invalid discovery key"))?;
        mac.update(DISCOVERY_DOMAIN);
        mac.update(&bytes);
        bytes.extend_from_slice(&mac.finalize().into_bytes());
        self.decode_discovery(
            slot,
            &bytes,
            protection.namespace,
            &protection.root_object_id,
            protection.key,
        )?;
        let name = Self::discovery_name(slot);
        let path = self.root_path.join(GENERATIONS_DIR_NAME).join(name);
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        match self.generations_dir.open_with(name, &options) {
            Ok(mut existing) => {
                let len = check_opened_file_security(&existing, &path)?;
                if len <= MAX_DISCOVERY_BYTES as u64 {
                    let old_bytes = read_file_bounded_exact(
                        &mut existing,
                        len,
                        MAX_DISCOVERY_BYTES as u64,
                        &path,
                    )?;
                    if old_bytes == bytes {
                        return sync_adopted_file(&existing, &self.generations_dir, &path);
                    }
                    if let Ok(old) = self.decode_discovery(
                        slot,
                        &old_bytes,
                        protection.namespace,
                        &protection.root_object_id,
                        protection.key,
                    ) {
                        if old.generation >= discovery.generation {
                            return Err(PublicationError::InvalidDiscovery(
                                "discovery retry conflicts with committed generation",
                            ));
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(PublicationError::io(&path, error)),
        }
        Self::checkpoint_publication(protection.cx)?;
        let stage_name = generate_stage_name("discovery");
        let stage_path = self.root_path.join(GENERATIONS_DIR_NAME).join(&stage_name);
        let mut stage_options = OpenOptions::new();
        stage_options
            .read(true)
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            stage_options.mode(0o600);
        }
        let mut stage = self
            .generations_dir
            .open_with(&stage_name, &stage_options)
            .map_err(|error| PublicationError::io(&stage_path, error))?;
        stage
            .write_all(&bytes)
            .and_then(|_| stage.sync_all())
            .map_err(|error| PublicationError::io(&stage_path, error))?;
        stage
            .seek(SeekFrom::Start(0))
            .map_err(|error| PublicationError::io(&stage_path, error))?;
        let len = check_opened_file_security(&stage, &stage_path)?;
        let readback =
            read_file_bounded_exact(&mut stage, len, MAX_DISCOVERY_BYTES as u64, &stage_path)?;
        if readback != bytes {
            return Err(PublicationError::InvalidDiscovery(
                "discovery staging readback differs",
            ));
        }
        self.decode_discovery(
            slot,
            &readback,
            protection.namespace,
            &protection.root_object_id,
            protection.key,
        )?;
        #[cfg(unix)]
        {
            let named = self
                .generations_dir
                .symlink_metadata(&stage_name)
                .map_err(|error| PublicationError::io(&stage_path, error))?;
            let opened = stage
                .metadata()
                .map_err(|error| PublicationError::io(&stage_path, error))?;
            if named.dev() != opened.dev() || named.ino() != opened.ino() {
                return Err(PublicationError::InvalidDiscovery(
                    "discovery stage binding changed",
                ));
            }
        }
        Self::checkpoint_publication(protection.cx)?;
        self.generations_dir
            .rename(&stage_name, &self.generations_dir, name)
            .map_err(|error| PublicationError::io(&path, error))?;
        sync_directory(
            &self.generations_dir,
            &self.root_path.join(GENERATIONS_DIR_NAME),
        )
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Trivial verifier that accepts all structurally valid candidates.
    struct AcceptAllVerifier;
    impl RootVerifier for AcceptAllVerifier {
        type Error = std::io::Error;
        type Verified = RootSlotCandidate;

        fn verify_root(
            &self,
            candidate: &RootSlotCandidate,
            _store: &SnapshotPublicationStore,
        ) -> Result<Self::Verified, Self::Error> {
            Ok(candidate.clone())
        }
    }

    /// Verifier that requires specific objects to exist in the store.
    struct ObjectRequiringVerifier {
        required_objects: Vec<String>,
    }
    impl RootVerifier for ObjectRequiringVerifier {
        type Error = std::io::Error;
        type Verified = RootSlotCandidate;

        fn verify_root(
            &self,
            candidate: &RootSlotCandidate,
            store: &SnapshotPublicationStore,
        ) -> Result<Self::Verified, Self::Error> {
            for obj_id in &self.required_objects {
                if store.read_object(obj_id).is_err() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("required object {obj_id} missing"),
                    ));
                }
            }
            Ok(candidate.clone())
        }
    }

    #[test]
    fn test_object_publish_and_read_roundtrip() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();

        let payload_bytes = b"encrypted-recovery-payload-bytes-12345".to_vec();
        let sha = sha256_hex(&payload_bytes);
        let obj = RecoveryObjectPayload {
            object_id: "obj-001".to_string(),
            expected_sha256: sha.clone(),
            ciphertext_bytes: payload_bytes.clone(),
        };

        let receipt = store.publish_object(&obj).unwrap();
        assert_eq!(receipt.object_id, "obj-001");
        assert_eq!(receipt.sha256, sha);
        assert!(!receipt.was_already_present);

        let read_back = store.read_object("obj-001").unwrap();
        assert_eq!(read_back, payload_bytes);
    }

    #[test]
    fn protected_publication_reconciles_corrupt_committed_predecessor_before_cas() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let cx = Cx::for_testing();
        let key = b"publication-reconciliation-key";
        let root_id = [77; 32];
        let mut request = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "publisher".into(),
            predecessor: None,
            manifest_bytes: b"first".to_vec(),
            created_at_ms: 1000,
        };
        let publish = |request: &GenerationRootPublishRequest| {
            store.publish_repair_protected_generation_root(
                &cx,
                request,
                "namespace",
                root_id,
                key,
                &AcceptAllVerifier,
            )
        };
        let first = publish(&request).unwrap();
        request.generation = 2;
        request.predecessor = Some(PredecessorBinding {
            expected_generation: 1,
            expected_hash: first.sha256.clone(),
        });
        request.manifest_bytes = b"second".to_vec();
        let second = publish(&request).unwrap();
        let first_bytes = std::fs::read(&first.path).unwrap();
        let discovery_path = temp
            .path()
            .join(GENERATIONS_DIR_NAME)
            .join(DISCOVERY_SLOT_B);
        let discovery_bytes = std::fs::read(&discovery_path).unwrap();
        std::fs::write(&second.path, b"torn committed root").unwrap();
        let torn_bytes = std::fs::read(&second.path).unwrap();

        request.manifest_bytes = b"conflicting second".to_vec();
        assert!(matches!(
            publish(&request),
            Err(PublicationError::GenerationConflict { generation: 2, .. })
        ));
        assert_eq!(std::fs::read(&second.path).unwrap(), torn_bytes);
        assert_eq!(std::fs::read(&discovery_path).unwrap(), discovery_bytes);
        assert_eq!(std::fs::read(&first.path).unwrap(), first_bytes);

        request.generation = 3;
        request.predecessor = Some(PredecessorBinding {
            expected_generation: 2,
            expected_hash: second.sha256,
        });
        request.manifest_bytes = b"third".to_vec();
        let third = publish(&request).expect("recovered committed gen2 is the CAS predecessor");
        assert_eq!(third.slot, first.slot);
        assert_eq!(std::fs::read(&second.path).unwrap(), torn_bytes);
        assert_eq!(std::fs::read(&discovery_path).unwrap(), discovery_bytes);
        let (discoveries, diagnostics) = store
            .inspect_repair_discovery("namespace", &root_id, key)
            .unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(
            discoveries.iter().map(|d| d.generation).collect::<Vec<_>>(),
            vec![3, 2]
        );
    }

    #[test]
    fn protected_root_discovery_repairs_corrupt_root_from_fresh_store() {
        use crate::snapshot_repair::decode_repair_object;
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let cx = Cx::for_testing();
        let key = b"independent-repair-key";
        let root_id = [41; 32];
        let request = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "publisher".to_owned(),
            predecessor: None,
            manifest_bytes: b"opaque authenticated root manifest".to_vec(),
            created_at_ms: 1000,
        };
        let receipt = store
            .publish_repair_protected_generation_root(
                &cx,
                &request,
                "trusted-namespace",
                root_id,
                key,
                &AcceptAllVerifier,
            )
            .unwrap();
        let outer = std::fs::read(&receipt.path).unwrap();
        let (discoveries, diagnostics) = store
            .inspect_repair_discovery("trusted-namespace", &root_id, key)
            .unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(discoveries.len(), 1);
        let discovery = &discoveries[0];
        let candidate = store.decode_recovered_root(receipt.slot, &outer).unwrap();
        assert!(
            store
                .root_candidate_matches_discovery(&candidate, discovery)
                .unwrap()
        );
        let expected = ExpectedRecoveryIdentity::new(discovery.outer_envelope_sha256, root_id, 1);
        let descriptor = store.read_repair_descriptor(&expected, key).unwrap();
        let chunk = &descriptor.chunks[0];
        let records_path = temp
            .path()
            .join(OBJECTS_DIR_NAME)
            .join(format!("{}.obj", chunk.records_object_id));
        let mut records = std::fs::read(&records_path).unwrap();
        records[0] ^= 0xff;
        std::fs::write(&records_path, records).unwrap();
        std::fs::write(&receipt.path, b"corrupt root framing").unwrap();
        drop(store);

        let fresh =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let (discovered, _) = fresh
            .inspect_repair_discovery("trusted-namespace", &root_id, key)
            .unwrap();
        let discovery = &discovered[0];
        let descriptor_bytes = fresh
            .read_repair_descriptor_bytes(&discovery.repair)
            .unwrap();
        let descriptor = RepairObjectDescriptor::from_authenticated_bytes(
            &descriptor_bytes,
            &expected,
            key,
            RepairObjectLimits::default(),
        )
        .unwrap();
        let repaired = decode_repair_object(
            &cx,
            &descriptor,
            &expected,
            key,
            shared_admission_controller(),
            RepairObjectLimits::default(),
            |chunk| {
                fresh
                    .read_repair_records(chunk)
                    .map_err(|error| RepairError::Storage(error.to_string()))
            },
        )
        .unwrap();
        assert_eq!(repaired.reconstructed_envelope, outer);
        let repaired_candidate = fresh
            .decode_recovered_root(discovery.slot, &repaired.reconstructed_envelope)
            .unwrap();
        assert!(
            fresh
                .root_candidate_matches_discovery(&repaired_candidate, discovery)
                .unwrap()
        );
        let (wrong_key, diagnostics) = fresh
            .inspect_repair_discovery("trusted-namespace", &root_id, b"wrong-key")
            .unwrap();
        assert!(wrong_key.is_empty());
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn protected_discovery_retry_fsyncs_and_preserves_prior_slot() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let cx = Cx::for_testing();
        let key = b"independent-repair-key";
        let root_id = [42; 32];
        let mut request = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "publisher".to_owned(),
            predecessor: None,
            manifest_bytes: b"first".to_vec(),
            created_at_ms: 1000,
        };
        let first = store
            .publish_repair_protected_generation_root(
                &cx,
                &request,
                "namespace",
                root_id,
                key,
                &AcceptAllVerifier,
            )
            .unwrap();
        let prior_path = temp
            .path()
            .join(GENERATIONS_DIR_NAME)
            .join(DISCOVERY_SLOT_A);
        let prior_bytes = std::fs::read(&prior_path).unwrap();
        request.generation = 2;
        request.predecessor = Some(PredecessorBinding {
            expected_generation: 1,
            expected_hash: first.sha256,
        });
        request.manifest_bytes = b"second".to_vec();
        let directory = temp.path().join(GENERATIONS_DIR_NAME);
        DIRECTORY_SYNC_FAILURE.with(|failure| *failure.borrow_mut() = Some(directory.clone()));
        assert!(matches!(
            store.publish_repair_protected_generation_root(
                &cx,
                &request,
                "namespace",
                root_id,
                key,
                &AcceptAllVerifier
            ),
            Err(PublicationError::Io { .. })
        ));
        DIRECTORY_SYNC_FAILURE.with(|failure| *failure.borrow_mut() = Some(directory));
        assert!(matches!(
            store.publish_repair_protected_generation_root(
                &cx,
                &request,
                "namespace",
                root_id,
                key,
                &AcceptAllVerifier
            ),
            Err(PublicationError::Io { .. })
        ));
        store
            .publish_repair_protected_generation_root(
                &cx,
                &request,
                "namespace",
                root_id,
                key,
                &AcceptAllVerifier,
            )
            .unwrap();
        assert_eq!(std::fs::read(prior_path).unwrap(), prior_bytes);
        std::fs::write(
            temp.path()
                .join(GENERATIONS_DIR_NAME)
                .join(DISCOVERY_SLOT_B),
            b"torn discovery",
        )
        .unwrap();
        let (discoveries, diagnostics) = store
            .inspect_repair_discovery("namespace", &root_id, key)
            .unwrap();
        assert_eq!(discoveries.len(), 1);
        assert_eq!(discoveries[0].generation, 1);
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn rejected_root_never_publishes_discovery() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let request = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "publisher".to_owned(),
            predecessor: None,
            manifest_bytes: b"uncommitted".to_vec(),
            created_at_ms: 1000,
        };
        let verifier = ObjectRequiringVerifier {
            required_objects: vec!["missing-required-object".to_owned()],
        };
        assert!(matches!(
            store.publish_repair_protected_generation_root(
                &Cx::for_testing(),
                &request,
                "namespace",
                [43; 32],
                b"repair-key",
                &verifier
            ),
            Err(PublicationError::VerificationRejected { .. })
        ));
        let (discovery, _) = store
            .inspect_repair_discovery("namespace", &[43; 32], b"repair-key")
            .unwrap();
        assert!(discovery.is_empty());
    }

    #[test]
    fn test_object_publish_idempotent_adoption() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();

        let payload = b"identical-payload-for-adoption".to_vec();
        let sha = sha256_hex(&payload);
        let obj = RecoveryObjectPayload {
            object_id: "idempotent-obj".to_string(),
            expected_sha256: sha.clone(),
            ciphertext_bytes: payload,
        };

        let r1 = store.publish_object(&obj).unwrap();
        assert!(!r1.was_already_present);

        let r2 = store.publish_object(&obj).unwrap();
        assert!(r2.was_already_present);
        assert_eq!(r1.sha256, r2.sha256);
    }

    #[test]
    fn object_adoption_retries_failed_directory_durability() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let bytes = b"rename-visible-but-sync-failed".to_vec();
        let object = RecoveryObjectPayload {
            object_id: "sync-retry".to_owned(),
            expected_sha256: sha256_hex(&bytes),
            ciphertext_bytes: bytes.clone(),
        };
        let directory = temp.path().join(OBJECTS_DIR_NAME);
        DIRECTORY_SYNC_FAILURE.with(|failure| *failure.borrow_mut() = Some(directory.clone()));
        assert!(matches!(
            store.publish_object(&object),
            Err(PublicationError::Io { .. })
        ));
        assert_eq!(store.read_object("sync-retry").unwrap(), bytes);

        // The visible rename is not a durability receipt. Even exact adoption
        // must fail if its own completion of the directory sync fails.
        DIRECTORY_SYNC_FAILURE.with(|failure| *failure.borrow_mut() = Some(directory));
        assert!(matches!(
            store.publish_object(&object),
            Err(PublicationError::Io { .. })
        ));
        assert!(store.publish_object(&object).unwrap().was_already_present);
    }

    #[test]
    fn store_open_retry_completes_child_directory_durability() {
        let temp = TempDir::new().unwrap();
        DIRECTORY_SYNC_FAILURE
            .with(|failure| *failure.borrow_mut() = Some(temp.path().to_path_buf()));
        assert!(matches!(
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()),
            Err(PublicationError::Io { .. })
        ));
        assert!(temp.path().join(OBJECTS_DIR_NAME).is_dir());
        DIRECTORY_SYNC_FAILURE
            .with(|failure| *failure.borrow_mut() = Some(temp.path().to_path_buf()));
        assert!(matches!(
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()),
            Err(PublicationError::Io { .. })
        ));
        SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
    }

    #[test]
    fn directory_scan_limit_counts_ignored_staging_entries() {
        let temp = TempDir::new().unwrap();
        let limits = PublicationLimits {
            max_dir_entries: 1,
            ..PublicationLimits::default()
        };
        let store = SnapshotPublicationStore::open(temp.path(), limits).unwrap();
        std::fs::write(
            temp.path().join(OBJECTS_DIR_NAME).join(".stage-one"),
            b"one",
        )
        .unwrap();
        std::fs::write(
            temp.path().join(OBJECTS_DIR_NAME).join(".stage-two"),
            b"two",
        )
        .unwrap();
        assert!(matches!(
            store.list_object_ids(),
            Err(PublicationError::ExcessiveDirectoryEntries { max_entries: 1 })
        ));
    }

    #[test]
    fn generation_retry_completes_failed_directory_durability() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let request = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "sync-retry".to_owned(),
            predecessor: None,
            manifest_bytes: b"visible-root".to_vec(),
            created_at_ms: 1000,
        };
        let directory = temp.path().join(ROOTS_DIR_NAME);
        DIRECTORY_SYNC_FAILURE.with(|failure| *failure.borrow_mut() = Some(directory.clone()));
        assert!(matches!(
            store.publish_generation_root(&request, &AcceptAllVerifier),
            Err(PublicationError::Io { .. })
        ));
        assert_eq!(
            store
                .select_verified_roots(&AcceptAllVerifier)
                .unwrap()
                .current
                .unwrap()
                .generation,
            1
        );
        DIRECTORY_SYNC_FAILURE.with(|failure| *failure.borrow_mut() = Some(directory));
        assert!(matches!(
            store.publish_generation_root(&request, &AcceptAllVerifier),
            Err(PublicationError::Io { .. })
        ));
        assert_eq!(
            store
                .publish_generation_root(&request, &AcceptAllVerifier)
                .unwrap()
                .generation,
            1
        );
    }

    #[test]
    fn test_object_publish_conflict_rejected() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();

        let p1 = b"original-content".to_vec();
        let obj1 = RecoveryObjectPayload {
            object_id: "conflicting-obj".to_string(),
            expected_sha256: sha256_hex(&p1),
            ciphertext_bytes: p1,
        };
        store.publish_object(&obj1).unwrap();

        let p2 = b"conflicting-content".to_vec();
        let obj2 = RecoveryObjectPayload {
            object_id: "conflicting-obj".to_string(),
            expected_sha256: sha256_hex(&p2),
            ciphertext_bytes: p2,
        };
        let err = store.publish_object(&obj2).unwrap_err();
        match err {
            PublicationError::ObjectConflict { object_id, .. } => {
                assert_eq!(object_id, "conflicting-obj");
            }
            other => panic!("expected ObjectConflict, got: {other:?}"),
        }
    }

    #[test]
    fn test_object_path_traversal_rejected() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();

        let obj = RecoveryObjectPayload {
            object_id: "../escape".to_string(),
            expected_sha256: sha256_hex(b"abc"),
            ciphertext_bytes: b"abc".to_vec(),
        };
        assert!(matches!(
            store.publish_object(&obj).unwrap_err(),
            PublicationError::InvalidObjectId { .. }
        ));
    }

    #[test]
    fn duplicate_verified_generations_are_not_selected_arbitrarily() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let request = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "test".to_string(),
            predecessor: None,
            manifest_bytes: b"generation-one".to_vec(),
            created_at_ms: 1,
        };
        store
            .publish_generation_root(&request, &AcceptAllVerifier)
            .unwrap();
        std::fs::copy(
            temp.path().join(ROOTS_DIR_NAME).join(ROOT_SLOT_A_NAME),
            temp.path().join(ROOTS_DIR_NAME).join(ROOT_SLOT_B_NAME),
        )
        .unwrap();
        assert!(matches!(
            store.select_verified_roots(&AcceptAllVerifier),
            Err(PublicationError::AmbiguousGeneration { generation: 1 })
        ));
        assert!(matches!(
            store.publish_generation_root(&request, &AcceptAllVerifier),
            Err(PublicationError::AmbiguousGeneration { generation: 1 })
        ));
    }

    #[test]
    fn generation_zero_is_rejected_before_publication() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let request = GenerationRootPublishRequest {
            generation: 0,
            publisher_id: "test".to_string(),
            predecessor: None,
            manifest_bytes: b"invalid-zero-generation".to_vec(),
            created_at_ms: 1,
        };
        assert!(matches!(
            store.publish_generation_root(&request, &AcceptAllVerifier),
            Err(PublicationError::InvalidGeneration)
        ));
        assert!(store.inspect_root_candidates().unwrap().0.is_empty());
    }

    #[test]
    fn test_dual_slot_progression_and_selection() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let verifier = AcceptAllVerifier;

        // 1. Initial generation (gen 1) publishes to Slot A
        let req1 = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "host:pid:1".to_string(),
            predecessor: None,
            manifest_bytes: b"manifest-gen-1".to_vec(),
            created_at_ms: 1000,
        };
        let r1 = store.publish_generation_root(&req1, &verifier).unwrap();
        assert_eq!(r1.generation, 1);
        assert_eq!(r1.slot, RootSlot::SlotA);

        let sel1 = store.select_verified_roots(&verifier).unwrap();
        assert_eq!(sel1.current.as_ref().unwrap().generation, 1);
        assert!(sel1.previous.is_none());

        // 2. Next generation (gen 2) publishes to Slot B, alternating
        let req2 = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "host:pid:1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: r1.sha256.clone(),
            }),
            manifest_bytes: b"manifest-gen-2".to_vec(),
            created_at_ms: 2000,
        };
        let r2 = store.publish_generation_root(&req2, &verifier).unwrap();
        assert_eq!(r2.generation, 2);
        assert_eq!(r2.slot, RootSlot::SlotB);

        let sel2 = store.select_verified_roots(&verifier).unwrap();
        assert_eq!(sel2.current.as_ref().unwrap().generation, 2);
        assert_eq!(sel2.previous.as_ref().unwrap().generation, 1);

        // 3. Next generation (gen 3) publishes back to Slot A
        let req3 = GenerationRootPublishRequest {
            generation: 3,
            publisher_id: "host:pid:1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 2,
                expected_hash: r2.sha256.clone(),
            }),
            manifest_bytes: b"manifest-gen-3".to_vec(),
            created_at_ms: 3000,
        };
        let r3 = store.publish_generation_root(&req3, &verifier).unwrap();
        assert_eq!(r3.generation, 3);
        assert_eq!(r3.slot, RootSlot::SlotA);

        let sel3 = store.select_verified_roots(&verifier).unwrap();
        assert_eq!(sel3.current.as_ref().unwrap().generation, 3);
        assert_eq!(sel3.previous.as_ref().unwrap().generation, 2);
    }

    #[test]
    fn test_predecessor_mismatch_fails_closed() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let verifier = AcceptAllVerifier;

        let req1 = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "host:pid:1".to_string(),
            predecessor: None,
            manifest_bytes: b"manifest-gen-1".to_vec(),
            created_at_ms: 1000,
        };
        store.publish_generation_root(&req1, &verifier).unwrap();

        // Attempt gen 2 with wrong predecessor hash
        let req2 = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "host:pid:1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: "wrong-sha256-digest".to_string(),
            }),
            manifest_bytes: b"manifest-gen-2".to_vec(),
            created_at_ms: 2000,
        };
        let err = store.publish_generation_root(&req2, &verifier).unwrap_err();
        assert!(matches!(err, PublicationError::PredecessorMismatch { .. }));
    }

    #[test]
    fn test_torn_newest_root_falls_back_to_intact_predecessor() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let verifier = AcceptAllVerifier;

        // Publish gen 1 into Slot A
        let req1 = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "host:pid:1".to_string(),
            predecessor: None,
            manifest_bytes: b"manifest-gen-1".to_vec(),
            created_at_ms: 1000,
        };
        store.publish_generation_root(&req1, &verifier).unwrap();

        // Publish gen 2 into Slot B
        let req2 = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "host:pid:1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: sha256_hex(b"manifest-gen-1"),
            }),
            manifest_bytes: b"manifest-gen-2".to_vec(),
            created_at_ms: 2000,
        };
        store.publish_generation_root(&req2, &verifier).unwrap();

        // Simulate a torn / interrupted write in Slot B (overwrite with truncated junk)
        let slot_b_path = temp.path().join(ROOTS_DIR_NAME).join(ROOT_SLOT_B_NAME);
        std::fs::write(&slot_b_path, b"FTRECROOTv1\0\0\0\0\0torn-partial-write").unwrap();

        // Selection must safely reject Slot B and fall back to Slot A (Gen 1)
        let sel = store.select_verified_roots(&verifier).unwrap();
        assert_eq!(sel.current.as_ref().unwrap().generation, 1);
        assert!(sel.previous.is_none());
        assert!(!sel.torn_or_rejected.is_empty());
        assert_eq!(sel.torn_or_rejected[0].slot, RootSlot::SlotB);

        // Verify that the torn file was NOT deleted (no deletion policy)
        assert!(slot_b_path.exists());
    }

    #[test]
    fn test_verifier_rejection_falls_back_to_prior_generation() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();

        // Publish required object for Gen 1
        let obj_payload = b"obj-data-for-gen1".to_vec();
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: "required-obj-1".to_string(),
                expected_sha256: sha256_hex(&obj_payload),
                ciphertext_bytes: obj_payload,
            })
            .unwrap();

        let v_accept = AcceptAllVerifier;
        // Gen 1
        let req1 = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "host:pid:1".to_string(),
            predecessor: None,
            manifest_bytes: b"manifest-gen-1".to_vec(),
            created_at_ms: 1000,
        };
        store.publish_generation_root(&req1, &v_accept).unwrap();

        // Gen 2 references missing object
        let req2 = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "host:pid:1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: sha256_hex(b"manifest-gen-1"),
            }),
            manifest_bytes: b"manifest-gen-2-needs-missing-obj".to_vec(),
            created_at_ms: 2000,
        };
        store.publish_generation_root(&req2, &v_accept).unwrap();

        // Now run selector with verifier that requires missing object
        let strict_verifier = ObjectRequiringVerifier {
            required_objects: vec!["missing-nonexistent-object".to_string()],
        };

        let sel = store.select_verified_roots(&strict_verifier).unwrap();
        // Since missing object is not present, both roots fail verification
        assert!(sel.current.is_none());
        assert_eq!(sel.torn_or_rejected.len(), 2);
    }

    #[test]
    fn test_has_object_and_list_objects() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();

        assert!(!store.has_object("obj-alpha").unwrap());

        let p1 = b"alpha".to_vec();
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: "obj-alpha".to_string(),
                expected_sha256: sha256_hex(&p1),
                ciphertext_bytes: p1,
            })
            .unwrap();

        assert!(store.has_object("obj-alpha").unwrap());
        assert!(!store.has_object("obj-beta").unwrap());

        let p2 = b"beta".to_vec();
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: "obj-beta".to_string(),
                expected_sha256: sha256_hex(&p2),
                ciphertext_bytes: p2,
            })
            .unwrap();

        let list = store.list_object_ids().unwrap();
        assert_eq!(list, vec!["obj-alpha".to_string(), "obj-beta".to_string()]);
    }

    #[test]
    fn test_verifier_rejection_of_newest_falls_back_to_valid_predecessor() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();

        // Publish object for Gen 1
        let obj_payload = b"obj-data-for-gen1".to_vec();
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: "obj-gen-1".to_string(),
                expected_sha256: sha256_hex(&obj_payload),
                ciphertext_bytes: obj_payload,
            })
            .unwrap();

        let v_accept = AcceptAllVerifier;
        // Gen 1
        let req1 = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "host:pid:1".to_string(),
            predecessor: None,
            manifest_bytes: b"manifest-gen-1:obj-gen-1".to_vec(),
            created_at_ms: 1000,
        };
        store.publish_generation_root(&req1, &v_accept).unwrap();

        // Gen 2
        let req2 = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "host:pid:1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: sha256_hex(b"manifest-gen-1:obj-gen-1"),
            }),
            manifest_bytes: b"manifest-gen-2:missing-obj-2".to_vec(),
            created_at_ms: 2000,
        };
        store.publish_generation_root(&req2, &v_accept).unwrap();

        // Verifier checks manifest contents: parses "manifest-gen-X:<obj-id>" and checks if obj exists
        struct ManifestObjectVerifier;
        impl RootVerifier for ManifestObjectVerifier {
            type Error = std::io::Error;
            type Verified = u64;

            fn verify_root(
                &self,
                candidate: &RootSlotCandidate,
                store: &SnapshotPublicationStore,
            ) -> Result<Self::Verified, Self::Error> {
                let s = String::from_utf8_lossy(&candidate.manifest_bytes);
                let parts: Vec<&str> = s.split(':').collect();
                if parts.len() == 2 {
                    let obj_id = parts[1];
                    store.read_object(obj_id)?;
                }
                Ok(candidate.generation)
            }
        }

        let sel = store
            .select_verified_roots(&ManifestObjectVerifier)
            .unwrap();
        // Gen 2 failed verification because missing-obj-2 was not found.
        // Gen 1 succeeded because obj-gen-1 exists!
        assert_eq!(sel.current, Some(1));
        assert!(sel.previous.is_none());
        assert_eq!(sel.torn_or_rejected.len(), 1);
        assert_eq!(sel.torn_or_rejected[0].generation, Some(2));
        assert_eq!(sel.torn_or_rejected[0].slot, RootSlot::SlotB);
    }

    #[test]
    fn test_negative_no_half_published_root_accepted_and_last_good_never_overwritten() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let verifier = AcceptAllVerifier;

        // 1. Publish Gen 1 into Slot A
        let req1 = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "publisher-primary".to_string(),
            predecessor: None,
            manifest_bytes: b"clean-gen-1-content".to_vec(),
            created_at_ms: 1000,
        };
        let r1 = store.publish_generation_root(&req1, &verifier).unwrap();
        assert_eq!(r1.slot, RootSlot::SlotA);

        let slot_a_path = temp.path().join(ROOTS_DIR_NAME).join(ROOT_SLOT_A_NAME);
        let slot_a_bytes_before = std::fs::read(&slot_a_path).unwrap();

        // 2. Simulate an aborted / interrupted / half-published write into Slot B
        // Write a truncated envelope missing the trailer SHA-256 and trailing payload bytes
        let slot_b_path = temp.path().join(ROOTS_DIR_NAME).join(ROOT_SLOT_B_NAME);
        let mut half_written_envelope = Vec::new();
        half_written_envelope.extend_from_slice(ENVELOPE_MAGIC);
        half_written_envelope.extend_from_slice(&10u32.to_le_bytes()); // header len claims 10 bytes
        half_written_envelope.extend_from_slice(b"incomplete"); // cut off before valid JSON & trailer
        std::fs::write(&slot_b_path, &half_written_envelope).unwrap();

        // Prove: half-published root in Slot B is NOT accepted as current or previous
        let sel = store.select_verified_roots(&verifier).unwrap();
        assert_eq!(
            sel.current.as_ref().unwrap().generation,
            1,
            "intact Gen 1 must remain active current root"
        );
        assert!(sel.previous.is_none());
        assert_eq!(sel.torn_or_rejected.len(), 1);
        assert_eq!(sel.torn_or_rejected[0].slot, RootSlot::SlotB);

        // Prove: last-good root (Slot A) was NEVER overwritten or altered
        let slot_a_bytes_after = std::fs::read(&slot_a_path).unwrap();
        assert_eq!(
            slot_a_bytes_before, slot_a_bytes_after,
            "last good root in Slot A must be bit-for-bit identical"
        );

        // Prove: incomplete / half-published file in Slot B is preserved on disk (no deletion)
        assert!(
            slot_b_path.exists(),
            "torn/half-published slot must be preserved on disk for forensics"
        );
    }

    #[test]
    fn test_non_monotonic_generation_rejected() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let verifier = AcceptAllVerifier;

        let req1 = GenerationRootPublishRequest {
            generation: 5,
            publisher_id: "pub-1".to_string(),
            predecessor: None,
            manifest_bytes: b"gen-5".to_vec(),
            created_at_ms: 1000,
        };
        store.publish_generation_root(&req1, &verifier).unwrap();

        // Attempt publishing gen 4 (less than active 5)
        let req_stale = GenerationRootPublishRequest {
            generation: 4,
            publisher_id: "pub-1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 5,
                expected_hash: sha256_hex(b"gen-5"),
            }),
            manifest_bytes: b"gen-4".to_vec(),
            created_at_ms: 2000,
        };
        let err = store
            .publish_generation_root(&req_stale, &verifier)
            .unwrap_err();
        assert!(matches!(
            err,
            PublicationError::NonMonotonicGeneration {
                candidate: 4,
                active: 5
            }
        ));
    }

    #[test]
    fn test_missing_predecessor_binding_rejected() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let verifier = AcceptAllVerifier;

        // Gen 2 without predecessor binding
        let req2 = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "pub-1".to_string(),
            predecessor: None,
            manifest_bytes: b"gen-2".to_vec(),
            created_at_ms: 1000,
        };
        let err = store.publish_generation_root(&req2, &verifier).unwrap_err();
        assert!(matches!(
            err,
            PublicationError::MissingPredecessorBinding { generation: 2 }
        ));
    }

    #[test]
    fn test_invalid_proposal_preserves_both_roots() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let v_accept = AcceptAllVerifier;

        // 1. Publish Gen 1 into Slot A
        let req1 = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "host:pid:1".to_string(),
            predecessor: None,
            manifest_bytes: b"gen-1-valid-manifest".to_vec(),
            created_at_ms: 1000,
        };
        let r1 = store.publish_generation_root(&req1, &v_accept).unwrap();
        assert_eq!(r1.slot, RootSlot::SlotA);

        // 2. Publish Gen 2 into Slot B
        let req2 = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "host:pid:1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: r1.sha256.clone(),
            }),
            manifest_bytes: b"gen-2-valid-manifest".to_vec(),
            created_at_ms: 2000,
        };
        let r2 = store.publish_generation_root(&req2, &v_accept).unwrap();
        assert_eq!(r2.slot, RootSlot::SlotB);

        // Snapshot bytes of both slots
        let slot_a_path = temp.path().join(ROOTS_DIR_NAME).join(ROOT_SLOT_A_NAME);
        let slot_b_path = temp.path().join(ROOTS_DIR_NAME).join(ROOT_SLOT_B_NAME);
        let slot_a_bytes_before = std::fs::read(&slot_a_path).unwrap();
        let slot_b_bytes_before = std::fs::read(&slot_b_path).unwrap();

        // 3. Propose Gen 3 targeting inactive Slot A, but with a verifier that rejects it!
        struct RejectingVerifier;
        impl RootVerifier for RejectingVerifier {
            type Error = std::io::Error;
            type Verified = RootSlotCandidate;
            fn verify_root(
                &self,
                candidate: &RootSlotCandidate,
                _store: &SnapshotPublicationStore,
            ) -> Result<Self::Verified, Self::Error> {
                if candidate.generation == 3 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "signature verification failed or missing object closure",
                    ));
                }
                Ok(candidate.clone())
            }
        }

        let req3 = GenerationRootPublishRequest {
            generation: 3,
            publisher_id: "host:pid:1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 2,
                expected_hash: r2.sha256.clone(),
            }),
            manifest_bytes: b"gen-3-invalid-payload".to_vec(),
            created_at_ms: 3000,
        };

        let err = store
            .publish_generation_root(&req3, &RejectingVerifier)
            .unwrap_err();
        assert!(matches!(err, PublicationError::VerificationRejected { .. }));

        // Prove: neither Slot A nor Slot B was modified! Both roots remain intact!
        let slot_a_bytes_after = std::fs::read(&slot_a_path).unwrap();
        let slot_b_bytes_after = std::fs::read(&slot_b_path).unwrap();
        assert_eq!(
            slot_a_bytes_before, slot_a_bytes_after,
            "Slot A (valid fallback Gen 1) must not be clobbered by bad candidate"
        );
        assert_eq!(
            slot_b_bytes_before, slot_b_bytes_after,
            "Slot B (active Gen 2) must remain intact"
        );

        // Prove: reader still selects Gen 2 as current and Gen 1 as previous
        let sel = store.select_verified_roots(&v_accept).unwrap();
        assert_eq!(sel.current.as_ref().unwrap().generation, 2);
        assert_eq!(sel.previous.as_ref().unwrap().generation, 1);
    }

    #[test]
    fn test_concurrent_publishers_and_lock_serialization() {
        use std::sync::Arc;
        let temp = TempDir::new().unwrap();
        let store = Arc::new(
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap(),
        );

        // Concurrent object publication with identical content
        let mut handles = Vec::new();
        for _ in 0..4 {
            let store_clone = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                let p = b"concurrent-identical-payload".to_vec();
                let obj = RecoveryObjectPayload {
                    object_id: "shared-obj".to_string(),
                    expected_sha256: sha256_hex(&p),
                    ciphertext_bytes: p,
                };
                store_clone.publish_object(&obj)
            }));
        }

        let mut already_present_count = 0;
        let mut newly_published_count = 0;
        for h in handles {
            let receipt = h.join().unwrap().unwrap();
            if receipt.was_already_present {
                already_present_count += 1;
            } else {
                newly_published_count += 1;
            }
        }
        assert_eq!(
            newly_published_count, 1,
            "exactly one publisher should create the object"
        );
        assert_eq!(
            already_present_count, 3,
            "other publishers should adopt idempotently"
        );

        let read_back = store.read_object("shared-obj").unwrap();
        assert_eq!(read_back, b"concurrent-identical-payload");
    }

    #[test]
    fn test_concurrent_publisher_double_advance_race_prevented() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let verifier = AcceptAllVerifier;

        // Gen 1 published in Slot A
        let req1 = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "pub-A".to_string(),
            predecessor: None,
            manifest_bytes: b"gen-1".to_vec(),
            created_at_ms: 1000,
        };
        let r1 = store.publish_generation_root(&req1, &verifier).unwrap();

        // Stale Publisher A prepares Gen 2 proposal based on Gen 1
        let stale_req_gen2 = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "pub-A".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: r1.sha256.clone(),
            }),
            manifest_bytes: b"gen-2-from-stale-pub-A".to_vec(),
            created_at_ms: 2000,
        };

        // Another Publisher B advances twice: Gen 2 (Slot B) then Gen 3 (Slot A)
        let req2_b = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "pub-B".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: r1.sha256.clone(),
            }),
            manifest_bytes: b"gen-2-pub-B".to_vec(),
            created_at_ms: 2000,
        };
        let r2_b = store.publish_generation_root(&req2_b, &verifier).unwrap();

        let req3_b = GenerationRootPublishRequest {
            generation: 3,
            publisher_id: "pub-B".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 2,
                expected_hash: r2_b.sha256.clone(),
            }),
            manifest_bytes: b"gen-3-pub-B".to_vec(),
            created_at_ms: 3000,
        };
        store.publish_generation_root(&req3_b, &verifier).unwrap();

        // Now active root is Gen 3 (Slot A), predecessor is Gen 2 (Slot B).
        // Stale Publisher A now tries to publish stale_req_gen2:
        // Inside publication lock, CAS / predecessor check detects active is Gen 3, NOT Gen 1!
        let err = store
            .publish_generation_root(&stale_req_gen2, &verifier)
            .unwrap_err();
        assert!(matches!(
            err,
            PublicationError::PredecessorMismatch { .. }
                | PublicationError::NonMonotonicGeneration { .. }
        ));

        // Both Slot A (Gen 3) and Slot B (Gen 2) remain untouched!
        let sel = store.select_verified_roots(&verifier).unwrap();
        assert_eq!(sel.current.as_ref().unwrap().generation, 3);
        assert_eq!(sel.previous.as_ref().unwrap().generation, 2);
    }

    #[test]
    fn test_symlink_root_rejected_without_following_or_mutating() {
        let temp = TempDir::new().unwrap();
        let target_dir = temp.path().join("real_target_dir");
        std::fs::create_dir_all(&target_dir).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&target_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            let symlink_path = temp.path().join("symlink_to_target");
            std::os::unix::fs::symlink(&target_dir, &symlink_path).unwrap();

            let err = SnapshotPublicationStore::open(&symlink_path, PublicationLimits::default())
                .unwrap_err();
            assert!(matches!(err, PublicationError::InsecurePermissions { .. }));

            // Prove: real target directory permissions were NOT modified (never chmod'd)
            let meta = std::fs::metadata(&target_dir).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        }
    }

    #[test]
    fn test_untrusted_permissions_rejected_without_chmod() {
        let temp = TempDir::new().unwrap();
        let untrusted_dir = temp.path().join("untrusted_dir");
        std::fs::create_dir_all(&untrusted_dir).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&untrusted_dir, std::fs::Permissions::from_mode(0o777))
                .unwrap();

            let err = SnapshotPublicationStore::open(&untrusted_dir, PublicationLimits::default())
                .unwrap_err();
            assert!(matches!(err, PublicationError::InsecurePermissions { .. }));

            // Prove: untrusted directory was NOT chmod'd to 0700; stays 0777
            let meta = std::fs::metadata(&untrusted_dir).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o777);
        }
    }

    #[test]
    fn test_near_limit_manifest_publication_and_read_admitted() {
        let temp = TempDir::new().unwrap();
        // Set a small manifest limit to easily test near-limit and over-limit
        let limits = PublicationLimits {
            max_root_manifest_bytes: 4096,
            max_object_bytes: 64 * 1024,
            max_dir_entries: 1024,
            max_error_records: 64,
        };
        let store = SnapshotPublicationStore::open(temp.path(), limits).unwrap();
        let verifier = AcceptAllVerifier;

        // Exactly at max_root_manifest_bytes (4096 bytes)
        let exact_manifest = vec![0xabu8; 4096];
        let req = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "pub-limit".to_string(),
            predecessor: None,
            manifest_bytes: exact_manifest.clone(),
            created_at_ms: 1000,
        };

        // Must succeed! Envelope size will be 4096 + framing overhead > 4096.
        let receipt = store.publish_generation_root(&req, &verifier).unwrap();
        assert_eq!(receipt.generation, 1);
        assert!(receipt.byte_len > 4096);

        // Crucial verification: inspect and select MUST admit and decode the envelope!
        let (candidates, diags) = store.inspect_root_candidates().unwrap();
        assert!(
            diags.is_empty(),
            "diags should be empty but found: {diags:?}"
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].manifest_bytes, exact_manifest);

        let selection = store.select_verified_roots(&verifier).unwrap();
        assert_eq!(selection.current.unwrap().manifest_bytes, exact_manifest);

        // One byte over max_root_manifest_bytes (4097 bytes) must be rejected
        let over_manifest = vec![0xabu8; 4097];
        let over_req = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "pub-over".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: receipt.sha256,
            }),
            manifest_bytes: over_manifest,
            created_at_ms: 2000,
        };
        let err = store
            .publish_generation_root(&over_req, &verifier)
            .unwrap_err();
        assert!(matches!(
            err,
            PublicationError::OversizedPayload {
                max_bytes: 4096,
                actual_bytes: 4097
            }
        ));
    }

    #[test]
    fn test_bounded_read_prevents_filegrowth_memory_exhaustion() {
        use std::io::Cursor;

        // 1. File grew concurrently beyond expected length (within max_bytes)
        let data = vec![42u8; 100];
        let mut cursor = Cursor::new(data);
        let err = read_file_bounded_exact(&mut cursor, 50, 200, Path::new("/test/growing.obj"))
            .unwrap_err();
        assert!(matches!(
            err,
            PublicationError::FileLengthChanged {
                expected: 50,
                actual: 100,
                ..
            }
        ));

        // 2. File shrank concurrently below expected length
        let data = vec![42u8; 30];
        let mut cursor = Cursor::new(data);
        let err = read_file_bounded_exact(&mut cursor, 50, 200, Path::new("/test/shrinking.obj"))
            .unwrap_err();
        assert!(matches!(
            err,
            PublicationError::FileLengthChanged {
                expected: 50,
                actual: 30,
                ..
            }
        ));

        // 3. File grew beyond max_bytes: limiter stops reading at max_bytes + 1
        let huge_data = vec![7u8; 10_000];
        let mut cursor = Cursor::new(huge_data);
        let err =
            read_file_bounded_exact(&mut cursor, 50, 100, Path::new("/test/huge.obj")).unwrap_err();
        assert!(matches!(
            err,
            PublicationError::OversizedPayload {
                max_bytes: 100,
                actual_bytes: 101,
            }
        ));

        // 4. Expected length itself exceeds max_bytes before read
        let data = vec![1u8; 50];
        let mut cursor = Cursor::new(data);
        let err = read_file_bounded_exact(&mut cursor, 500, 100, Path::new("/test/oversized.obj"))
            .unwrap_err();
        assert!(matches!(
            err,
            PublicationError::OversizedPayload {
                max_bytes: 100,
                actual_bytes: 500,
            }
        ));

        // 5. Exact match succeeds
        let data = vec![99u8; 64];
        let mut cursor = Cursor::new(data);
        let res =
            read_file_bounded_exact(&mut cursor, 64, 100, Path::new("/test/exact.obj")).unwrap();
        assert_eq!(res.len(), 64);
    }

    #[test]
    fn test_lost_reply_retry_reconciles_idempotently_before_stale_predecessor_rejection() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let verifier = AcceptAllVerifier;

        // Publish Gen 1
        let req1 = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "pub-1".to_string(),
            predecessor: None,
            manifest_bytes: b"gen-1-data".to_vec(),
            created_at_ms: 1000,
        };
        let r1 = store.publish_generation_root(&req1, &verifier).unwrap();

        // Publish Gen 2 pointing to Gen 1
        let req2 = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "pub-1".to_string(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 1,
                expected_hash: r1.sha256.clone(),
            }),
            manifest_bytes: b"gen-2-data".to_vec(),
            created_at_ms: 2000,
        };
        let r2 = store.publish_generation_root(&req2, &verifier).unwrap();
        assert_eq!(r2.generation, 2);
        assert_eq!(r2.slot, RootSlot::SlotB);

        // Simulate lost-reply retry: publisher retries exact same request for Gen 2
        // Even though active root on disk is now Gen 2 (not Gen 1), it must reconcile
        // idempotently BEFORE stale predecessor rejection!
        let r2_retry = store.publish_generation_root(&req2, &verifier).unwrap();
        assert_eq!(r2_retry.generation, 2);
        assert_eq!(r2_retry.slot, RootSlot::SlotB);
        assert_eq!(r2_retry.sha256, r2.sha256);

        let mut changed_timestamp = req2.clone();
        changed_timestamp.created_at_ms += 1;
        assert!(matches!(
            store.publish_generation_root(&changed_timestamp, &verifier),
            Err(PublicationError::GenerationConflict { .. })
        ));

        // However, a retry with conflicting manifest content must be rejected!
        let mut conflicting_req2 = req2.clone();
        conflicting_req2.manifest_bytes = b"gen-2-CONFLICTING-data".to_vec();
        let err = store
            .publish_generation_root(&conflicting_req2, &verifier)
            .unwrap_err();
        assert!(matches!(err, PublicationError::GenerationConflict { .. }));

        // Both roots remain intact and verified
        let sel = store.select_verified_roots(&verifier).unwrap();
        assert_eq!(sel.current.as_ref().unwrap().generation, 2);
        assert_eq!(sel.previous.as_ref().unwrap().generation, 1);
    }

    #[test]
    fn test_object_publish_noreplace_and_exact_existing_adoption() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();

        let payload = b"immutable-blob-content".to_vec();
        let obj = RecoveryObjectPayload {
            object_id: "obj-noreplace-01".to_string(),
            expected_sha256: sha256_hex(&payload),
            ciphertext_bytes: payload.clone(),
        };

        // 1. Initial publication
        let r1 = store.publish_object(&obj).unwrap();
        assert!(!r1.was_already_present);

        // 2. Second publication with identical payload: adopted idempotently without overwrite
        let r2 = store.publish_object(&obj).unwrap();
        assert!(r2.was_already_present);
        assert_eq!(r2.sha256, r1.sha256);

        // 3. Third publication with conflicting payload: fails closed with ObjectConflict
        let conflicting_payload = b"evil-replacement-content".to_vec();
        let conflicting_obj = RecoveryObjectPayload {
            object_id: "obj-noreplace-01".to_string(),
            expected_sha256: sha256_hex(&conflicting_payload),
            ciphertext_bytes: conflicting_payload,
        };
        let err = store.publish_object(&conflicting_obj).unwrap_err();
        assert!(matches!(err, PublicationError::ObjectConflict { .. }));

        // 4. Verify original content was never overwritten
        let read_back = store.read_object("obj-noreplace-01").unwrap();
        assert_eq!(read_back, payload);
    }

    #[test]
    fn publication_lock_contention_returns_without_waiting() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();
        let first = store.acquire_publication_lock().unwrap();
        assert!(matches!(
            store.acquire_publication_lock(),
            Err(PublicationError::PublicationBusy)
        ));
        drop(first);
        let _next = store.acquire_publication_lock().unwrap();
    }

    #[test]
    fn test_lock_inode_revalidation_detects_replaced_file() {
        let temp = TempDir::new().unwrap();
        let store =
            SnapshotPublicationStore::open(temp.path(), PublicationLimits::default()).unwrap();

        // Acquire lock
        let lock1 = store.acquire_publication_lock().unwrap();

        // Simulate lock file replacement while lock was held/blocking
        let lock_path = temp.path().join(".publication.lock");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let meta1 = std::fs::metadata(&lock_path).unwrap();

            // Atomically replace lock file with new inode (tests inode change without file deletion)
            let replacement_path = temp.path().join(".publication.lock.replacement");
            let _ = std::fs::File::create(&replacement_path).unwrap();
            std::fs::rename(&replacement_path, &lock_path).unwrap();
            let meta2 = std::fs::metadata(&lock_path).unwrap();
            assert_ne!(
                meta1.ino(),
                meta2.ino(),
                "new file should have different inode"
            );
        }

        // Release lock1
        drop(lock1);

        // Next acquire must succeed and bind to current inode
        let lock2 = store.acquire_publication_lock().unwrap();
        drop(lock2);
    }
}
