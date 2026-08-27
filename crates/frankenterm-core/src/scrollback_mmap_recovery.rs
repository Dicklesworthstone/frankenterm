//! Crash-safe scrollback recovery — orphan-file scanner.
//!
//! **Beads:** [BR-TERM-EMULATOR-UPLIFT-2.5.2] — two parallel
//! bead IDs cover this substrate:
//! - `ft-5te6x` — session decomposition (this module's primary
//!   bead; closed earlier this session).
//! - `ft-2okh0.5.2` — canonical decomposition of parent epic
//!   `ft-2okh0.5`. Closed via cross-reference to ft-5te6x.
//!
//! Same cross-reference pattern as `ft-2okh0.5.1 ↔ ft-kscfg`
//! and `ft-2okh0.3.1 ↔ ft-d0ol8`. The bead-ID mapping table
//! lives at the parent design doc.
//!
//! **Design doc:** [`docs/design/crash-safe-scrollback.md`]
//! (../../../../docs/design/crash-safe-scrollback.md).
//! **Format types:** [`crate::scrollback_mmap_format`]
//! (shipped under ft-kscfg / ft-2okh0.5.1).
//!
//! # What this module ships
//!
//! The pure orphan-detection layer of the recovery flow. Walks a
//! scrollback directory (typically `~/.local/share/ft/scrollback/`),
//! reads each `.bin` file's 256-byte header via
//! [`crate::scrollback_mmap_format::ScrollbackHeader::decode`],
//! and classifies every candidate into one of:
//!
//! - [`OrphanState::Orphaned`] — header valid, no live owner; the
//!   restore prompt should offer this candidate.
//! - [`OrphanState::Locked`] — `.lock` file held by a live owner;
//!   skip without opening the live data file.
//! - [`OrphanState::Corrupt`] — header decode failed; the operator
//!   gets a structured error reason via the embedded
//!   [`HeaderDecodeError`].
//! - [`OrphanState::Unsafe`] — a symlink, hard link, non-private leaf,
//!   owner/identity mismatch, oversized file, unsafe lock, or filename/header
//!   mismatch; never eligible for recovery or discard.
//! - [`OrphanState::WrongShape`] — file exists but isn't a
//!   `<pane_uuid>.bin` (e.g. someone dropped a `.txt` next to the
//!   scrollback files); skip without raising.
//!
//! # What still lives outside this scanner
//!
//! - The CLI commands `ft session list-orphans`, `ft session recover
//!   <id>`, and `ft session discard <id>` are wired in
//!   `crates/frankenterm/src/main.rs`; production CLI scanning uses
//!   [`FlockLockProbe`] so live writers remain locked and disabled.
//! - `ft session recover <id>` reads the orphaned mmap records, builds
//!   an exact UTF-8 export plan, reapplies redaction, and writes a new private
//!   transcript. It never sends historical output through live PTY input.
//! - This module owns the header-level classifier and export-plan accounting;
//!   executable live-pane attachment is intentionally unavailable.
//!
//! # Why ship the scanner first
//!
//! Orphan detection is the load-bearing decision for every other
//! recovery sub-task. The picker UI consumes
//! `Vec<OrphanCandidate>`; the CLI commands consume the same; the
//! kill-9 test fixture (ft-0ulxc) asserts that a known-uuid file
//! shows up in the scanner's output. Pinning the scan logic in
//! its own module, with synthetic-file unit tests, means each
//! follow-up implements against a stable contract.
//!
//! # Read-side flock semantic
//!
//! Per the design doc, `<pane_uuid>.bin.lock` is an `flock(LOCK_EX
//! | LOCK_NB)` advisory. A live ft instance holds the lock; an
//! orphan is a file whose lock is gone or whose lock is available
//! to a fresh `flock(LOCK_EX | LOCK_NB)` attempt. This module
//! exposes the lock-probe boundary as a [`LockProbe`] trait so
//! tests can substitute a deterministic probe while the CLI uses
//! [`FlockLockProbe`].

use crate::scrollback_mmap_format::{HEADER_SIZE, HeaderDecodeError, RecordKind, ScrollbackHeader};
use crate::scrollback_mmap_writer::{
    HARD_MAX_LINEAR_RECORD_FILE_BYTES, HARD_MAX_LINEAR_RECORD_PAYLOAD_BYTES,
    HARD_MAX_LINEAR_RECORDS, LinearRecordCompleteness, LinearRecordReadLimits,
    LinearRecordSnapshot, LinearRecordSourceIdentity, MmapScrollbackError,
    read_linear_records_in_directory,
};
use cap_fs_ext::{DirExt as _, FollowSymlinks, MetadataExt as _, OpenOptionsFollowExt as _};
use cap_std::fs::{Dir as CapDir, OpenOptions as CapOpenOptions};
use fs2::FileExt;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{ErrorKind, Read as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const DEFAULT_MAX_RECOVERY_DIRECTORY_ENTRIES: usize = 128;
pub const DEFAULT_MAX_RECOVERY_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_RECOVERY_RECORDS: usize = 262_144;
pub const DEFAULT_MAX_RECOVERY_REPLAY_CHUNKS: usize = 262_144;
pub const DEFAULT_MAX_RECOVERY_PAYLOAD_BYTES: u64 = 50 * 1024 * 1024;
pub const DEFAULT_MAX_RECOVERY_TRANSCRIPT_BYTES: usize = 50 * 1024 * 1024;

pub const HARD_MAX_RECOVERY_DIRECTORY_ENTRIES: usize = 1024;
pub const HARD_MAX_RECOVERY_FILE_BYTES: u64 = HARD_MAX_LINEAR_RECORD_FILE_BYTES;
pub const HARD_MAX_RECOVERY_RECORDS: usize = HARD_MAX_LINEAR_RECORDS;
pub const HARD_MAX_RECOVERY_REPLAY_CHUNKS: usize = 1_048_576;
pub const HARD_MAX_RECOVERY_PAYLOAD_BYTES: u64 = HARD_MAX_LINEAR_RECORD_PAYLOAD_BYTES;
pub const HARD_MAX_RECOVERY_TRANSCRIPT_BYTES: usize = 1024 * 1024 * 1024;

/// One explicit, caller-owned resource envelope for legacy orphan discovery,
/// record decoding, and transcript planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyRecoveryLimits {
    pub max_directory_entries: usize,
    pub max_file_bytes: u64,
    pub max_records: usize,
    pub max_replay_chunks: usize,
    pub max_payload_bytes: u64,
    pub max_transcript_bytes: usize,
}

impl LegacyRecoveryLimits {
    pub const DEFAULT: Self = Self {
        max_directory_entries: DEFAULT_MAX_RECOVERY_DIRECTORY_ENTRIES,
        max_file_bytes: DEFAULT_MAX_RECOVERY_FILE_BYTES,
        max_records: DEFAULT_MAX_RECOVERY_RECORDS,
        max_replay_chunks: DEFAULT_MAX_RECOVERY_REPLAY_CHUNKS,
        max_payload_bytes: DEFAULT_MAX_RECOVERY_PAYLOAD_BYTES,
        max_transcript_bytes: DEFAULT_MAX_RECOVERY_TRANSCRIPT_BYTES,
    };

    /// Validate that every caller-selected limit is non-zero, internally
    /// consistent, and no larger than the corresponding hard safety ceiling.
    pub fn validate(self) -> std::io::Result<Self> {
        let valid = self.max_directory_entries > 0
            && self.max_directory_entries <= HARD_MAX_RECOVERY_DIRECTORY_ENTRIES
            && self.max_file_bytes >= HEADER_SIZE as u64
            && self.max_file_bytes <= HARD_MAX_RECOVERY_FILE_BYTES
            && self.max_records > 0
            && self.max_records <= HARD_MAX_RECOVERY_RECORDS
            && self.max_replay_chunks > 0
            && self.max_replay_chunks <= HARD_MAX_RECOVERY_REPLAY_CHUNKS
            && self.max_payload_bytes > 0
            && self.max_payload_bytes <= HARD_MAX_RECOVERY_PAYLOAD_BYTES
            && self.max_payload_bytes <= self.max_file_bytes
            && self.max_transcript_bytes > 0
            && self.max_transcript_bytes <= HARD_MAX_RECOVERY_TRANSCRIPT_BYTES
            && u64::try_from(self.max_transcript_bytes)
                .is_ok_and(|bytes| bytes <= self.max_payload_bytes);
        if valid {
            Ok(self)
        } else {
            Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "legacy recovery limits must be non-zero, internally consistent, and within hard caps",
            ))
        }
    }

    const fn record_read_limits(self) -> LinearRecordReadLimits {
        LinearRecordReadLimits {
            max_file_bytes: self.max_file_bytes,
            max_records: self.max_records,
            max_payload_bytes: self.max_payload_bytes,
        }
    }
}

/// Classification of one candidate file in a scrollback directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanState {
    /// Header decoded cleanly and the file is not held by a live
    /// owner. Eligible for the restore prompt.
    Orphaned,
    /// `.lock` file is held by a live owner; another ft instance
    /// owns this scrollback. Skip without prompting.
    Locked,
    /// Header decode failed. The reason is in the
    /// `HeaderDecodeError` carried in [`OrphanCandidate::header`].
    /// Surface to the operator with the structured reason; the
    /// file is left in place so it can be hand-inspected.
    Corrupt,
    /// The candidate or its lock did not satisfy the private regular-file,
    /// identity, owner, or filename/header binding contract. Never selectable.
    Unsafe,
    /// File doesn't match the `<pane_uuid>.bin` shape. Common
    /// cause: an unrelated file dropped into the scrollback dir.
    /// Skipped without raising.
    WrongShape,
}

impl OrphanState {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Orphaned => "orphaned",
            Self::Locked => "locked",
            Self::Corrupt => "corrupt",
            Self::Unsafe => "unsafe",
            Self::WrongShape => "wrong_shape",
        }
    }
}

/// One candidate file from the scan. Always carries the absolute
/// path; carries the parsed header for an inspectable orphan. Locked files are
/// intentionally not opened because the scanner does not own their flock.
/// Operational candidates are deliberately linear: a held recovery lease
/// cannot be duplicated before a consuming discard.
///
/// ```compile_fail
/// use frankenterm_core::scrollback_mmap_recovery::OrphanCandidate;
/// fn duplicate(candidate: OrphanCandidate) {
///     let _copy = candidate.clone();
/// }
/// ```
#[derive(Debug)]
pub struct OrphanCandidate {
    pub path: PathBuf,
    pub state: OrphanState,
    /// `Ok(header)` when the header decoded cleanly,
    /// `Err(decode_error)` when [`Self::state`] is
    /// [`OrphanState::Corrupt`]. Always `None` for
    /// [`OrphanState::WrongShape`], [`OrphanState::Locked`], and
    /// [`OrphanState::Unsafe`].
    pub header: Option<Result<ScrollbackHeader, HeaderDecodeError>>,
    pub unsafe_reason: Option<OrphanUnsafeReason>,
    directory: Option<Arc<PinnedScrollbackDirectory>>,
    leaf: Option<PathBuf>,
    recovery_lease: Option<RecoveryLease>,
    data_identity: Option<LinearRecordSourceIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanUnsafeReason {
    NotRegularFile,
    HardLinked,
    NotPrivate,
    OwnerMismatch,
    Oversized,
    IdentityChanged,
    UnsafeLock,
    FilenameHeaderMismatch,
    MissingRecoveryLease,
    InvalidLimits,
}

impl OrphanUnsafeReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRegularFile => "not_regular_file",
            Self::HardLinked => "hard_linked",
            Self::NotPrivate => "not_private",
            Self::OwnerMismatch => "owner_mismatch",
            Self::Oversized => "oversized",
            Self::IdentityChanged => "identity_changed",
            Self::UnsafeLock => "unsafe_lock",
            Self::FilenameHeaderMismatch => "filename_header_mismatch",
            Self::MissingRecoveryLease => "missing_recovery_lease",
            Self::InvalidLimits => "invalid_limits",
        }
    }
}

/// Durable result of deleting exactly one identity-checked data leaf.
///
/// The private lock leaf is intentionally retained. Removing a locked inode
/// would let a writer create and lock a replacement inode before this
/// operation drops its lease, splitting the flock authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDiscardReceipt {
    pub data_path: PathBuf,
    pub retained_lock_path: PathBuf,
    pub directory_synced: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LegacyRecoveryDiscardError {
    #[error("legacy scrollback candidate {path} in state {state} is not discardable")]
    Ineligible { path: PathBuf, state: &'static str },
    #[error("legacy scrollback candidate {path} cannot be discarded safely: {reason}")]
    Unsafe { path: PathBuf, reason: &'static str },
    #[error("legacy scrollback discard {phase} failed for {path}: {source}")]
    Io {
        path: PathBuf,
        phase: &'static str,
        #[source]
        source: std::io::Error,
    },
}

impl OrphanCandidate {
    /// Construct an unleased classification for pure picker rendering.
    /// Operational recovery deliberately rejects candidates created through
    /// this seam because they do not carry a production lock lease.
    #[must_use]
    pub fn for_picker(
        path: PathBuf,
        state: OrphanState,
        header: Option<Result<ScrollbackHeader, HeaderDecodeError>>,
    ) -> Self {
        Self {
            path,
            state,
            header,
            unsafe_reason: None,
            directory: None,
            leaf: None,
            recovery_lease: None,
            data_identity: None,
        }
    }

    /// Convenience: extract the parsed header iff the candidate
    /// is in a state where the header was safely readable.
    #[must_use]
    pub fn header_ok(&self) -> Option<&ScrollbackHeader> {
        match (&self.state, &self.header) {
            (OrphanState::Orphaned, Some(Ok(header))) => Some(header),
            _ => None,
        }
    }

    /// Convenience: extract the structured decode error iff
    /// [`Self::state`] is [`OrphanState::Corrupt`].
    #[must_use]
    pub fn corrupt_reason(&self) -> Option<&HeaderDecodeError> {
        match (&self.state, &self.header) {
            (OrphanState::Corrupt, Some(Err(err))) => Some(err),
            _ => None,
        }
    }

    #[must_use]
    pub const fn unsafe_reason(&self) -> Option<OrphanUnsafeReason> {
        self.unsafe_reason
    }

    /// Decode this candidate while retaining the production flock acquired
    /// during classification. Test-only/unleased probes cannot cross this
    /// boundary.
    pub fn read_records(
        &self,
        limits: LegacyRecoveryLimits,
    ) -> Result<LinearRecordSnapshot, MmapScrollbackError> {
        let limits = limits
            .validate()
            .map_err(|_| MmapScrollbackError::InvalidReadLimit {
                limit_name: "legacy_recovery_envelope",
            })?;
        if !matches!(&self.state, OrphanState::Orphaned) {
            return Err(MmapScrollbackError::UnsafeReadSource {
                path: self.path.clone(),
                reason: "candidate is not an orphan",
            });
        }
        let directory =
            self.directory
                .as_ref()
                .ok_or_else(|| MmapScrollbackError::UnsafeReadSource {
                    path: self.path.clone(),
                    reason: "candidate has no pinned directory",
                })?;
        let leaf = self
            .leaf
            .as_ref()
            .ok_or_else(|| MmapScrollbackError::UnsafeReadSource {
                path: self.path.clone(),
                reason: "candidate has no pinned leaf",
            })?;
        let lease =
            self.recovery_lease
                .as_ref()
                .ok_or_else(|| MmapScrollbackError::UnsafeReadSource {
                    path: self.path.clone(),
                    reason: "candidate has no held recovery lease",
                })?;
        let expected_identity =
            self.data_identity
                .ok_or_else(|| MmapScrollbackError::UnsafeReadSource {
                    path: self.path.clone(),
                    reason: "candidate has no classified data identity",
                })?;
        if directory.display_path.join(leaf) != self.path {
            return Err(MmapScrollbackError::UnsafeReadSource {
                path: self.path.clone(),
                reason: "candidate display path no longer matches its pinned data capability",
            });
        }
        directory
            .revalidate()
            .map_err(|_| MmapScrollbackError::UnsafeReadSource {
                path: self.path.clone(),
                reason: "pinned scrollback directory changed identity",
            })?;
        lease
            .revalidate()
            .map_err(|_| MmapScrollbackError::UnsafeReadSource {
                path: self.path.clone(),
                reason: "recovery lock changed identity",
            })?;
        let snapshot = read_linear_records_in_directory(
            &directory.directory,
            leaf,
            &self.path,
            limits.record_read_limits(),
        )?;
        if self.header_ok().copied() != Some(snapshot.header)
            || !header_matches_filename(leaf, &snapshot.header)
            || snapshot.source_identity != expected_identity
        {
            return Err(MmapScrollbackError::UnsafeReadSource {
                path: self.path.clone(),
                reason: "filename/header identity changed during recovery",
            });
        }
        lease
            .revalidate()
            .map_err(|_| MmapScrollbackError::UnsafeReadSource {
                path: self.path.clone(),
                reason: "recovery lock changed identity after read",
            })?;
        directory
            .revalidate()
            .map_err(|_| MmapScrollbackError::UnsafeReadSource {
                path: self.path.clone(),
                reason: "pinned scrollback directory changed identity after read",
            })?;
        Ok(snapshot)
    }

    /// Delete exactly this candidate's identity-checked data leaf while the
    /// production recovery lease remains held, then fsync the pinned parent.
    /// Only leased orphaned/corrupt candidates produced by the hardened
    /// scanner can cross this boundary. The reusable private lock leaf remains
    /// in place to avoid a new-inode flock split-brain window.
    pub fn discard(self) -> Result<LegacyDiscardReceipt, LegacyRecoveryDiscardError> {
        if !matches!(&self.state, OrphanState::Orphaned | OrphanState::Corrupt) {
            return Err(LegacyRecoveryDiscardError::Ineligible {
                path: self.path,
                state: self.state.as_str(),
            });
        }

        let path = self.path;
        let directory = self
            .directory
            .ok_or_else(|| LegacyRecoveryDiscardError::Unsafe {
                path: path.clone(),
                reason: "candidate has no pinned directory",
            })?;
        let leaf = self
            .leaf
            .ok_or_else(|| LegacyRecoveryDiscardError::Unsafe {
                path: path.clone(),
                reason: "candidate has no pinned data leaf",
            })?;
        let lease = self
            .recovery_lease
            .ok_or_else(|| LegacyRecoveryDiscardError::Unsafe {
                path: path.clone(),
                reason: "candidate has no held recovery lease",
            })?;
        let expected_identity =
            self.data_identity
                .ok_or_else(|| LegacyRecoveryDiscardError::Unsafe {
                    path: path.clone(),
                    reason: "candidate has no classified data identity",
                })?;
        if directory.display_path.join(&leaf) != path {
            return Err(LegacyRecoveryDiscardError::Unsafe {
                path,
                reason: "candidate display path no longer matches its pinned data capability",
            });
        }

        directory
            .revalidate()
            .map_err(|source| LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "directory revalidation",
                source,
            })?;
        lease
            .revalidate()
            .map_err(|source| LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "lease revalidation",
                source,
            })?;
        let directory_metadata = directory.directory.dir_metadata().map_err(|source| {
            LegacyRecoveryDiscardError::Io {
                path: directory.display_path.clone(),
                phase: "directory metadata",
                source,
            }
        })?;
        validate_private_directory_metadata(&directory_metadata).map_err(|source| {
            LegacyRecoveryDiscardError::Io {
                path: directory.display_path.clone(),
                phase: "directory privacy validation",
                source,
            }
        })?;
        let path_metadata = directory
            .directory
            .symlink_metadata(&leaf)
            .map_err(|source| LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "data metadata",
                source,
            })?;
        validate_candidate_metadata(&path_metadata, &directory_metadata).map_err(|reason| {
            LegacyRecoveryDiscardError::Unsafe {
                path: path.clone(),
                reason: reason.as_str(),
            }
        })?;
        if metadata_identity(&path_metadata) != expected_identity {
            return Err(LegacyRecoveryDiscardError::Unsafe {
                path,
                reason: "candidate data identity changed before discard",
            });
        }

        let mut options = CapOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let data_file = directory
            .directory
            .open_with(&leaf, &options)
            .map_err(|source| LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "data descriptor open",
                source,
            })?
            .into_std();
        let handle_metadata =
            data_file
                .metadata()
                .map_err(|source| LegacyRecoveryDiscardError::Io {
                    path: path.clone(),
                    phase: "data descriptor metadata",
                    source,
                })?;
        validate_private_pair(&path_metadata, &handle_metadata).map_err(|source| {
            LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "data descriptor validation",
                source,
            }
        })?;
        if metadata_identity(&handle_metadata) != expected_identity {
            return Err(LegacyRecoveryDiscardError::Unsafe {
                path,
                reason: "opened data identity differs from classified candidate",
            });
        }

        // This is the last named-leaf census before capability-relative
        // unlink. The private effective-UID-owned parent is the trust boundary;
        // keeping both data and lock descriptors open closes cross-user races.
        directory
            .revalidate()
            .map_err(|source| LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "final directory revalidation",
                source,
            })?;
        lease
            .revalidate()
            .map_err(|source| LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "final lease revalidation",
                source,
            })?;
        let final_path_metadata =
            directory
                .directory
                .symlink_metadata(&leaf)
                .map_err(|source| LegacyRecoveryDiscardError::Io {
                    path: path.clone(),
                    phase: "final data metadata",
                    source,
                })?;
        validate_candidate_metadata(&final_path_metadata, &directory_metadata).map_err(
            |reason| LegacyRecoveryDiscardError::Unsafe {
                path: path.clone(),
                reason: reason.as_str(),
            },
        )?;
        if metadata_identity(&final_path_metadata) != expected_identity {
            return Err(LegacyRecoveryDiscardError::Unsafe {
                path,
                reason: "candidate data identity changed immediately before discard",
            });
        }

        directory.directory.remove_file(&leaf).map_err(|source| {
            LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "data unlink",
                source,
            }
        })?;
        match directory.directory.symlink_metadata(&leaf) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LegacyRecoveryDiscardError::Io {
                    path,
                    phase: "post-unlink data census",
                    source,
                });
            }
            Ok(_) => {
                return Err(LegacyRecoveryDiscardError::Unsafe {
                    path,
                    reason: "data leaf was replaced during discard",
                });
            }
        }
        let handle_metadata_after =
            data_file
                .metadata()
                .map_err(|source| LegacyRecoveryDiscardError::Io {
                    path: path.clone(),
                    phase: "post-unlink descriptor metadata",
                    source,
                })?;
        if metadata_identity(&handle_metadata_after) != expected_identity {
            return Err(LegacyRecoveryDiscardError::Unsafe {
                path,
                reason: "opened data identity changed during discard",
            });
        }
        if let Err(source) = directory
            .directory
            .open(".")
            .and_then(|handle| handle.sync_all())
        {
            if !matches!(source.raw_os_error(), Some(9 | 22 | 95)) {
                return Err(LegacyRecoveryDiscardError::Io {
                    path: directory.display_path.clone(),
                    phase: "directory fsync",
                    source,
                });
            }
        }
        lease
            .revalidate()
            .map_err(|source| LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "post-unlink lease revalidation",
                source,
            })?;
        directory
            .revalidate()
            .map_err(|source| LegacyRecoveryDiscardError::Io {
                path: path.clone(),
                phase: "post-unlink directory revalidation",
                source,
            })?;

        Ok(LegacyDiscardReceipt {
            data_path: path,
            retained_lock_path: lease.0.lock_path.clone(),
            directory_synced: true,
        })
    }
}

/// Default maximum chunk size for replaying recovered mmap payloads through the
/// mux write API.
pub const DEFAULT_REPLAY_CHUNK_BYTES: usize = 4096;

/// Replay status derived from a decoded mmap record stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmapReplayStatus {
    /// The mmap file contained no replayable records and no skipped records.
    Empty,
    /// Every decoded record could be converted into an exact UTF-8 replay
    /// payload.
    Replayed,
    /// Some records can be replayed, but at least one record was skipped or
    /// the source ended before its header-declared committed cursor.
    Partial,
    /// Records were present but none could be replayed safely, or an incomplete
    /// source had no safely decoded records.
    Unreplayable,
}

impl MmapReplayStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Replayed => "replayed",
            Self::Partial => "partial",
            Self::Unreplayable => "unreplayable",
        }
    }
}

/// Why a decoded mmap record was not replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MmapReplaySkipReason {
    /// The mux write boundary accepts text. A non-UTF-8 payload would be
    /// lossy, so recovery leaves that record out and reports it.
    InvalidUtf8 {
        valid_up_to: usize,
        error_len: Option<usize>,
    },
}

impl MmapReplaySkipReason {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidUtf8 { .. } => "invalid_utf8",
        }
    }
}

/// A replay chunk ready for `MuxInterface::send_text_with_options` with
/// `no_newline=true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmapReplayChunk {
    pub record_index: usize,
    pub record_kind: RecordKind,
    pub text: String,
}

/// One decoded record that could not be represented exactly at the mux text
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmapReplaySkippedRecord {
    pub record_index: usize,
    pub record_kind: RecordKind,
    pub payload_bytes: usize,
    pub reason: MmapReplaySkipReason,
}

/// Per-record-kind replay accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmapReplayKindCount {
    pub record_kind: RecordKind,
    pub records: usize,
    pub payload_bytes: usize,
}

/// Exact replay plan for decoded mmap records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmapReplayPlan {
    /// Completeness of the header-declared source prefix. An incomplete source
    /// can yield useful replay chunks, but its overall status is never
    /// [`MmapReplayStatus::Replayed`] or [`MmapReplayStatus::Empty`].
    pub source_completeness: LinearRecordCompleteness,
    pub records_read: usize,
    pub records_replayed: usize,
    pub bytes_read: usize,
    pub bytes_replayed: usize,
    pub chunks: Vec<MmapReplayChunk>,
    pub skipped: Vec<MmapReplaySkippedRecord>,
    pub kind_counts: Vec<MmapReplayKindCount>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum MmapReplayPlanError {
    #[error("invalid legacy recovery resource limits")]
    InvalidLimits,
    #[error("legacy recovery {limit_name} limit {limit} exceeded by {observed}")]
    LimitExceeded {
        limit_name: &'static str,
        limit: u64,
        observed: u64,
    },
}

impl MmapReplayPlan {
    /// Build an exact UTF-8 replay plan from decoded mmap records.
    ///
    /// This deliberately does not insert separators between records. The mmap
    /// payloads are already the durable byte stream; recovery must not add
    /// synthetic newlines or terminal reset prefixes.
    pub fn from_snapshot(
        snapshot: LinearRecordSnapshot,
        chunk_size: usize,
        limits: LegacyRecoveryLimits,
    ) -> Result<Self, MmapReplayPlanError> {
        let limits = limits
            .validate()
            .map_err(|_| MmapReplayPlanError::InvalidLimits)?;
        let chunk_size = chunk_size.max(1);
        let LinearRecordSnapshot {
            records,
            completeness: source_completeness,
            ..
        } = snapshot;
        let mut plan = Self {
            source_completeness,
            records_read: 0,
            records_replayed: 0,
            bytes_read: 0,
            bytes_replayed: 0,
            chunks: Vec::new(),
            skipped: Vec::new(),
            kind_counts: Vec::new(),
        };

        for (record_index, (record_kind, payload)) in records.into_iter().enumerate() {
            let payload_bytes = payload.len();
            if plan.records_read >= limits.max_records {
                return Err(MmapReplayPlanError::LimitExceeded {
                    limit_name: "records",
                    limit: limits.max_records as u64,
                    observed: limits.max_records as u64 + 1,
                });
            }
            plan.records_read =
                plan.records_read
                    .checked_add(1)
                    .ok_or(MmapReplayPlanError::LimitExceeded {
                        limit_name: "records",
                        limit: limits.max_records as u64,
                        observed: u64::MAX,
                    })?;
            plan.bytes_read = plan.bytes_read.checked_add(payload_bytes).ok_or(
                MmapReplayPlanError::LimitExceeded {
                    limit_name: "payload_bytes",
                    limit: limits.max_payload_bytes,
                    observed: u64::MAX,
                },
            )?;
            if plan.bytes_read as u64 > limits.max_payload_bytes {
                return Err(MmapReplayPlanError::LimitExceeded {
                    limit_name: "payload_bytes",
                    limit: limits.max_payload_bytes,
                    observed: plan.bytes_read as u64,
                });
            }
            plan.bump_kind_count(record_kind, payload_bytes);

            match String::from_utf8(payload) {
                Ok(text) => {
                    plan.records_replayed = plan.records_replayed.checked_add(1).ok_or(
                        MmapReplayPlanError::LimitExceeded {
                            limit_name: "records",
                            limit: limits.max_records as u64,
                            observed: u64::MAX,
                        },
                    )?;
                    plan.bytes_replayed = plan.bytes_replayed.checked_add(payload_bytes).ok_or(
                        MmapReplayPlanError::LimitExceeded {
                            limit_name: "transcript_bytes",
                            limit: limits.max_transcript_bytes as u64,
                            observed: u64::MAX,
                        },
                    )?;
                    if plan.bytes_replayed > limits.max_transcript_bytes {
                        return Err(MmapReplayPlanError::LimitExceeded {
                            limit_name: "transcript_bytes",
                            limit: limits.max_transcript_bytes as u64,
                            observed: plan.bytes_replayed as u64,
                        });
                    }
                    push_replay_chunks(
                        &mut plan.chunks,
                        record_index,
                        record_kind,
                        &text,
                        chunk_size,
                        limits.max_replay_chunks,
                    )?;
                }
                Err(err) => {
                    let utf8 = err.utf8_error();
                    plan.skipped.push(MmapReplaySkippedRecord {
                        record_index,
                        record_kind,
                        payload_bytes,
                        reason: MmapReplaySkipReason::InvalidUtf8 {
                            valid_up_to: utf8.valid_up_to(),
                            error_len: utf8.error_len(),
                        },
                    });
                }
            }
        }

        Ok(plan)
    }

    #[must_use]
    pub fn status(&self) -> MmapReplayStatus {
        if !self.source_completeness.is_complete() {
            if self.records_replayed > 0 {
                MmapReplayStatus::Partial
            } else {
                MmapReplayStatus::Unreplayable
            }
        } else if self.records_read == 0 {
            MmapReplayStatus::Empty
        } else if self.records_replayed == self.records_read {
            MmapReplayStatus::Replayed
        } else if self.records_replayed > 0 {
            MmapReplayStatus::Partial
        } else {
            MmapReplayStatus::Unreplayable
        }
    }

    fn bump_kind_count(&mut self, record_kind: RecordKind, payload_bytes: usize) {
        if let Some(count) = self
            .kind_counts
            .iter_mut()
            .find(|count| count.record_kind == record_kind)
        {
            count.records += 1;
            count.payload_bytes += payload_bytes;
            return;
        }
        self.kind_counts.push(MmapReplayKindCount {
            record_kind,
            records: 1,
            payload_bytes,
        });
    }
}

#[must_use]
pub const fn replay_record_kind_label(record_kind: RecordKind) -> &'static str {
    match record_kind {
        RecordKind::Text => "text",
        RecordKind::Osc => "osc",
        RecordKind::Csi => "csi",
        RecordKind::Cursor => "cursor",
        RecordKind::Clear => "clear",
    }
}

fn push_replay_chunks(
    chunks: &mut Vec<MmapReplayChunk>,
    record_index: usize,
    record_kind: RecordKind,
    text: &str,
    chunk_size: usize,
    max_chunks: usize,
) -> Result<(), MmapReplayPlanError> {
    if text.is_empty() {
        return Ok(());
    }

    let mut start = 0;
    while start < text.len() {
        let mut end = (start + chunk_size).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(offset, _)| start + offset);
        }

        if chunks.len() >= max_chunks {
            return Err(MmapReplayPlanError::LimitExceeded {
                limit_name: "replay_chunks",
                limit: max_chunks as u64,
                observed: max_chunks as u64 + 1,
            });
        }
        chunks.push(MmapReplayChunk {
            record_index,
            record_kind,
            text: text[start..end].to_string(),
        });
        start = end;
    }
    Ok(())
}

/// Action the recovery picker will apply to selected candidates when
/// the operator confirms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Re-attach eligible orphaned scrollback files.
    Recover,
    /// Discard selected files. Corrupt files are selectable only in
    /// this mode because they cannot be recovered safely.
    Discard,
}

/// Decision emitted by the picker for each displayed candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// Recover the scrollback file at this path.
    Recover(PathBuf),
    /// Discard the scrollback file at this path.
    Discard(PathBuf),
    /// Leave this scrollback file untouched.
    Skip(PathBuf),
}

/// Keyboard input understood by the recovery picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanPickerKey {
    /// Move highlight one row up.
    Up,
    /// Move highlight one row down.
    Down,
    /// Toggle the highlighted row.
    Toggle,
    /// Confirm the current selection.
    Confirm,
    /// Cancel the picker.
    Cancel,
}

/// Result of applying one key to the picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanPickerOutcome {
    /// Picker remains open.
    Pending,
    /// Operator confirmed; decisions are ready for the caller.
    Confirmed(Vec<RecoveryDecision>),
    /// Operator cancelled; caller should leave every file untouched.
    Cancelled,
}

/// Badge shown next to one displayed picker row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanPickerBadge {
    /// Eligible orphan file.
    Orphaned,
    /// Held by another live owner; greyed out and never selectable.
    Locked,
    /// Header decode failed; shown with the structured reason.
    Corrupt,
    /// Candidate failed the private-file or identity contract.
    Unsafe,
}

impl OrphanPickerBadge {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Orphaned => "orphaned",
            Self::Locked => "locked",
            Self::Corrupt => "corrupt",
            Self::Unsafe => "unsafe",
        }
    }
}

/// One row in the interactive recovery picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanPickerRow {
    pub path: PathBuf,
    pub pane_uuid_short: String,
    pub created_at_epoch_ms: Option<u64>,
    pub bytes_written: Option<u64>,
    pub last_msync_age_ms: Option<u64>,
    pub badge: OrphanPickerBadge,
    pub corrupt_reason: Option<String>,
    pub selectable: bool,
    pub selected: bool,
    pub accessibility_label: String,
}

/// Pure picker state for scrollback-orphan recovery. This is the
/// UI-independent layer consumed by a terminal picker, a snapshot
/// test, or the later CLI command plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanPickerState {
    action: RecoveryAction,
    rows: Vec<OrphanPickerRow>,
    highlighted: Option<usize>,
}

impl OrphanPickerState {
    /// Build display rows from scanner output. Wrong-shape files are
    /// intentionally hidden; locked files are displayed but disabled.
    #[must_use]
    pub fn new(candidates: &[OrphanCandidate], action: RecoveryAction, now_epoch_ms: u64) -> Self {
        let rows: Vec<OrphanPickerRow> = candidates
            .iter()
            .filter_map(|candidate| row_from_candidate(candidate, action, now_epoch_ms))
            .collect();
        let highlighted = rows.first().map(|_| 0);
        Self {
            action,
            rows,
            highlighted,
        }
    }

    #[must_use]
    pub fn action(&self) -> RecoveryAction {
        self.action
    }

    #[must_use]
    pub fn rows(&self) -> &[OrphanPickerRow] {
        &self.rows
    }

    #[must_use]
    pub fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    #[must_use]
    pub fn highlighted_row(&self) -> Option<&OrphanPickerRow> {
        self.highlighted.and_then(|idx| self.rows.get(idx))
    }

    /// Move highlight one displayed row up, saturating at the top.
    pub fn move_up(&mut self) {
        if let Some(idx) = self.highlighted {
            self.highlighted = Some(idx.saturating_sub(1));
        }
    }

    /// Move highlight one displayed row down, saturating at the last
    /// visible row.
    pub fn move_down(&mut self) {
        if let Some(idx) = self.highlighted {
            let last = self.rows.len().saturating_sub(1);
            self.highlighted = Some((idx + 1).min(last));
        }
    }

    /// Toggle the highlighted row. Disabled rows are left unchanged.
    pub fn toggle_highlighted(&mut self) {
        let Some(idx) = self.highlighted else {
            return;
        };
        let Some(row) = self.rows.get_mut(idx) else {
            return;
        };
        if row.selectable {
            row.selected = !row.selected;
        }
    }

    /// Convert the current selection into one decision per displayed
    /// row. Hidden wrong-shape files are deliberately absent.
    #[must_use]
    pub fn confirm(&self) -> Vec<RecoveryDecision> {
        self.rows
            .iter()
            .map(|row| {
                if row.selected {
                    match self.action {
                        RecoveryAction::Recover => RecoveryDecision::Recover(row.path.clone()),
                        RecoveryAction::Discard => RecoveryDecision::Discard(row.path.clone()),
                    }
                } else {
                    RecoveryDecision::Skip(row.path.clone())
                }
            })
            .collect()
    }

    /// Apply one keyboard action and return the resulting picker
    /// outcome. This maps directly to up/down, space, enter, and q/Esc.
    pub fn handle_key(&mut self, key: OrphanPickerKey) -> OrphanPickerOutcome {
        match key {
            OrphanPickerKey::Up => {
                self.move_up();
                OrphanPickerOutcome::Pending
            }
            OrphanPickerKey::Down => {
                self.move_down();
                OrphanPickerOutcome::Pending
            }
            OrphanPickerKey::Toggle => {
                self.toggle_highlighted();
                OrphanPickerOutcome::Pending
            }
            OrphanPickerKey::Confirm => OrphanPickerOutcome::Confirmed(self.confirm()),
            OrphanPickerKey::Cancel => OrphanPickerOutcome::Cancelled,
        }
    }
}

fn row_from_candidate(
    candidate: &OrphanCandidate,
    action: RecoveryAction,
    now_epoch_ms: u64,
) -> Option<OrphanPickerRow> {
    match &candidate.state {
        OrphanState::WrongShape => None,
        OrphanState::Orphaned => {
            let header = candidate.header_ok()?;
            let badge = OrphanPickerBadge::Orphaned;
            let selectable = true;
            let last_msync_age_ms = now_epoch_ms.checked_sub(header.last_msync_at_epoch_ms);
            let pane_uuid_short = pane_uuid_short_from_header(header);
            let accessibility_label = format_accessibility_label(
                &pane_uuid_short,
                badge,
                Some(header.total_bytes_written),
                last_msync_age_ms,
                None,
                selectable,
            );
            Some(OrphanPickerRow {
                path: candidate.path.clone(),
                pane_uuid_short,
                created_at_epoch_ms: Some(header.created_at_epoch_ms),
                bytes_written: Some(header.total_bytes_written),
                last_msync_age_ms,
                badge,
                corrupt_reason: None,
                selectable,
                selected: false,
                accessibility_label,
            })
        }
        OrphanState::Locked => {
            let header = candidate.header_ok();
            let pane_uuid_short = header.map_or_else(
                || pane_uuid_short_from_path(&candidate.path),
                pane_uuid_short_from_header,
            );
            let bytes_written = header.map(|header| header.total_bytes_written);
            let last_msync_age_ms =
                header.and_then(|header| now_epoch_ms.checked_sub(header.last_msync_at_epoch_ms));
            let accessibility_label = format_accessibility_label(
                &pane_uuid_short,
                OrphanPickerBadge::Locked,
                bytes_written,
                last_msync_age_ms,
                None,
                false,
            );
            Some(OrphanPickerRow {
                path: candidate.path.clone(),
                pane_uuid_short,
                created_at_epoch_ms: header.map(|header| header.created_at_epoch_ms),
                bytes_written,
                last_msync_age_ms,
                badge: OrphanPickerBadge::Locked,
                corrupt_reason: None,
                selectable: false,
                selected: false,
                accessibility_label,
            })
        }
        OrphanState::Corrupt => {
            let reason = candidate.corrupt_reason().map_or_else(
                || "unknown header decode error".to_string(),
                ToString::to_string,
            );
            let selectable = action == RecoveryAction::Discard;
            let pane_uuid_short = pane_uuid_short_from_path(&candidate.path);
            let accessibility_label = format_accessibility_label(
                &pane_uuid_short,
                OrphanPickerBadge::Corrupt,
                None,
                None,
                Some(&reason),
                selectable,
            );
            Some(OrphanPickerRow {
                path: candidate.path.clone(),
                pane_uuid_short,
                created_at_epoch_ms: None,
                bytes_written: None,
                last_msync_age_ms: None,
                badge: OrphanPickerBadge::Corrupt,
                corrupt_reason: Some(reason),
                selectable,
                selected: false,
                accessibility_label,
            })
        }
        OrphanState::Unsafe => {
            let reason = candidate.unsafe_reason.map_or_else(
                || "unknown".to_string(),
                |reason| reason.as_str().to_string(),
            );
            let pane_uuid_short = pane_uuid_short_from_path(&candidate.path);
            let accessibility_label = format_accessibility_label(
                &pane_uuid_short,
                OrphanPickerBadge::Unsafe,
                None,
                None,
                Some(&reason),
                false,
            );
            Some(OrphanPickerRow {
                path: candidate.path.clone(),
                pane_uuid_short,
                created_at_epoch_ms: None,
                bytes_written: None,
                last_msync_age_ms: None,
                badge: OrphanPickerBadge::Unsafe,
                corrupt_reason: Some(reason),
                selectable: false,
                selected: false,
                accessibility_label,
            })
        }
    }
}

fn pane_uuid_short_from_header(header: &ScrollbackHeader) -> String {
    let mut out = String::with_capacity(16);
    for byte in header.pane_uuid.iter().take(8) {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn pane_uuid_short_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.chars().take(16).collect())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_accessibility_label(
    pane_uuid_short: &str,
    badge: OrphanPickerBadge,
    bytes_written: Option<u64>,
    last_msync_age_ms: Option<u64>,
    corrupt_reason: Option<&str>,
    selectable: bool,
) -> String {
    let availability = if selectable { "selectable" } else { "disabled" };
    match badge {
        OrphanPickerBadge::Corrupt => format!(
            "scrollback orphan {pane_uuid_short}, corrupt, {availability}, reason: {}",
            corrupt_reason.unwrap_or("unknown")
        ),
        OrphanPickerBadge::Unsafe => format!(
            "scrollback orphan {pane_uuid_short}, unsafe, disabled, reason: {}",
            corrupt_reason.unwrap_or("unknown")
        ),
        OrphanPickerBadge::Locked => {
            let bytes =
                bytes_written.map_or_else(|| "unknown".to_string(), |value| value.to_string());
            let age = last_msync_age_ms
                .map_or_else(|| "unknown".to_string(), |value| format!("{value} ms"));
            format!(
                "scrollback orphan {pane_uuid_short}, locked, disabled, {bytes} bytes written, last sync age {age}"
            )
        }
        OrphanPickerBadge::Orphaned => format!(
            "scrollback orphan {pane_uuid_short}, orphaned, {availability}, {bytes} bytes written, last sync age {age} ms",
            bytes = bytes_written.unwrap_or(0),
            age = last_msync_age_ms.unwrap_or(0)
        ),
    }
}

fn metadata_identity(metadata: &impl cap_fs_ext::MetadataExt) -> LinearRecordSourceIdentity {
    LinearRecordSourceIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn validate_private_directory_metadata(metadata: &cap_std::fs::Metadata) -> std::io::Result<()> {
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "scrollback security boundary is not a directory",
        ));
    }
    #[cfg(unix)]
    {
        if cap_std::fs::MetadataExt::mode(metadata) & 0o7077 != 0 {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "scrollback directory permissions are not owner-private",
            ));
        }
        if cap_std::fs::MetadataExt::uid(metadata) != rustix::process::geteuid().as_raw() {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "scrollback directory is not owned by the effective user",
            ));
        }
    }
    Ok(())
}

struct PinnedScrollbackDirectory {
    directory: CapDir,
    display_path: PathBuf,
    identity: LinearRecordSourceIdentity,
}

impl std::fmt::Debug for PinnedScrollbackDirectory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PinnedScrollbackDirectory")
            .field("display_path", &self.display_path)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl PinnedScrollbackDirectory {
    fn open(path: &Path) -> std::io::Result<Self> {
        let directory = open_directory_tree_nofollow(path)?;
        let metadata = directory.dir_metadata()?;
        validate_private_directory_metadata(&metadata)?;
        Ok(Self {
            identity: metadata_identity(&metadata),
            directory,
            display_path: path.to_path_buf(),
        })
    }

    fn revalidate(&self) -> std::io::Result<()> {
        let reopened = open_directory_tree_nofollow(&self.display_path)?;
        let metadata = reopened.dir_metadata()?;
        validate_private_directory_metadata(&metadata)?;
        if metadata_identity(&metadata) != self.identity {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "scrollback directory identity changed",
            ));
        }
        Ok(())
    }
}

/// Single-owner flock authority carried by one operational candidate.
///
/// ```compile_fail
/// use frankenterm_core::scrollback_mmap_recovery::RecoveryLease;
/// fn duplicate(lease: RecoveryLease) {
///     let _copy = lease.clone();
/// }
/// ```
pub struct RecoveryLease(RecoveryLeaseInner);

struct RecoveryLeaseInner {
    lock_file: File,
    lock_path: PathBuf,
    lock_leaf: PathBuf,
    directory_path: PathBuf,
    directory_identity: LinearRecordSourceIdentity,
    lock_identity: LinearRecordSourceIdentity,
}

impl std::fmt::Debug for RecoveryLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveryLease")
            .field("lock_path", &self.0.lock_path)
            .field("directory_identity", &self.0.directory_identity)
            .field("lock_identity", &self.0.lock_identity)
            .finish_non_exhaustive()
    }
}

impl RecoveryLease {
    fn directory_identity(&self) -> LinearRecordSourceIdentity {
        self.0.directory_identity
    }

    fn revalidate(&self) -> std::io::Result<()> {
        let directory = open_directory_tree_nofollow(&self.0.directory_path)?;
        let directory_metadata = directory.dir_metadata()?;
        validate_private_directory_metadata(&directory_metadata)?;
        if metadata_identity(&directory_metadata) != self.0.directory_identity {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "recovery lock directory identity changed",
            ));
        }
        let path_metadata = directory.symlink_metadata(&self.0.lock_leaf)?;
        let handle_metadata = self.0.lock_file.metadata()?;
        validate_candidate_metadata(&path_metadata, &directory_metadata).map_err(|_| {
            std::io::Error::new(ErrorKind::InvalidData, "recovery lock is not private")
        })?;
        validate_private_pair(&path_metadata, &handle_metadata)?;
        if metadata_identity(&path_metadata) != self.0.lock_identity
            || metadata_identity(&handle_metadata) != self.0.lock_identity
        {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "recovery lock identity changed",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum LockProbeOutcome {
    Locked,
    Acquired(RecoveryLease),
    UnlockedWithoutLease,
    Unsafe,
}

/// Probe and, for a production orphan, retain an exclusive
/// `<pane_uuid>.bin.lock` lease. The retained lease is the authority that
/// closes writer-start races between classification and recovery.
pub trait LockProbe {
    fn probe(&self, lock_path: &Path) -> LockProbeOutcome;
}

/// Synchronous closure-shaped LockProbe. Useful for tests and for
/// embedding ad-hoc probes (e.g. environment-variable overrides).
impl<F> LockProbe for F
where
    F: Fn(&Path) -> bool,
{
    fn probe(&self, lock_path: &Path) -> LockProbeOutcome {
        if (self)(lock_path) {
            LockProbeOutcome::Locked
        } else {
            LockProbeOutcome::UnlockedWithoutLease
        }
    }
}

/// Default lock probe used when the caller has no override.
/// Returns `false` for every input — no lock = orphan. The
/// production CLI path uses [`FlockLockProbe`] instead; this fallback
/// is intentionally deterministic for tests and for callers that
/// explicitly choose best-effort offline scanning.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysOrphaned;

impl LockProbe for AlwaysOrphaned {
    fn probe(&self, _lock_path: &Path) -> LockProbeOutcome {
        LockProbeOutcome::UnlockedWithoutLease
    }
}

/// Production lock probe for CLI recovery scans.
///
/// The writer creates `<pane_uuid>.bin.lock` and holds an exclusive advisory
/// lock for its lifetime. Recovery opens or creates that same private leaf
/// without following symlinks and retains its exclusive lock in the returned
/// candidate until the selected operation finishes.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlockLockProbe;

impl LockProbe for FlockLockProbe {
    fn probe(&self, lock_path: &Path) -> LockProbeOutcome {
        acquire_recovery_lease(lock_path).unwrap_or(LockProbeOutcome::Unsafe)
    }
}

fn acquire_recovery_lease(lock_path: &Path) -> std::io::Result<LockProbeOutcome> {
    let lock_leaf = lock_path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "lock has no file name"))?;
    let directory_path = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let directory = open_directory_tree_nofollow(&directory_path)?;
    let directory_metadata = directory.dir_metadata()?;
    validate_private_directory_metadata(&directory_metadata)?;
    let directory_identity = metadata_identity(&directory_metadata);

    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let lock_file = directory.open_with(&lock_leaf, &options)?.into_std();
    let path_metadata = directory.symlink_metadata(&lock_leaf)?;
    let handle_metadata = lock_file.metadata()?;
    validate_candidate_metadata(&path_metadata, &directory_metadata)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "recovery lock is not private"))?;
    validate_private_pair(&path_metadata, &handle_metadata)?;
    let lock_identity = metadata_identity(&handle_metadata);
    if metadata_identity(&path_metadata) != lock_identity {
        return Ok(LockProbeOutcome::Unsafe);
    }

    match lock_file.try_lock_exclusive() {
        Ok(()) => {
            let path_metadata_after = directory.symlink_metadata(&lock_leaf)?;
            let handle_metadata_after = lock_file.metadata()?;
            validate_candidate_metadata(&path_metadata_after, &directory_metadata).map_err(
                |_| std::io::Error::new(ErrorKind::InvalidData, "recovery lock is not private"),
            )?;
            validate_private_pair(&path_metadata_after, &handle_metadata_after)?;
            if metadata_identity(&path_metadata_after) != lock_identity
                || metadata_identity(&handle_metadata_after) != lock_identity
            {
                return Ok(LockProbeOutcome::Unsafe);
            }
            Ok(LockProbeOutcome::Acquired(RecoveryLease(
                RecoveryLeaseInner {
                    lock_file,
                    lock_path: lock_path.to_path_buf(),
                    lock_leaf,
                    directory_path,
                    directory_identity,
                    lock_identity,
                },
            )))
        }
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(LockProbeOutcome::Locked),
        Err(_) => Ok(LockProbeOutcome::Unsafe),
    }
}

/// Walk `scrollback_dir` under a caller-provided finite census and classify
/// each regular file plus every correctly-shaped non-regular leaf. Directory
/// components and leaves are opened without following symlinks.
///
/// `lock_probe` is consulted before header inspection. A locked candidate's
/// data file is never opened. Production orphan candidates retain the acquired
/// flock through the returned [`OrphanCandidate`].
///
/// Directory enumeration failures and census overflow are surfaced as `Err`.
/// Candidate leaves and paired canonical lock companions each have an
/// independent `max_directory_entries` budget, and the total census is capped
/// at twice that value. This keeps a production scan from exhausting its own
/// next scan merely by creating one lock companion per admitted data leaf.
/// Header truncation is [`OrphanState::Corrupt`]; filesystem identity or
/// privacy failures are [`OrphanState::Unsafe`].
pub fn scan_orphans(
    scrollback_dir: &Path,
    lock_probe: &impl LockProbe,
    limits: LegacyRecoveryLimits,
) -> std::io::Result<Vec<OrphanCandidate>> {
    let limits = limits.validate()?;
    let directory = Arc::new(PinnedScrollbackDirectory::open(scrollback_dir)?);
    let mut out = Vec::new();
    let mut leaves = Vec::new();
    let total_leaf_limit = limits.max_directory_entries.checked_mul(2).ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            "legacy scrollback directory entry limit overflows",
        )
    })?;
    for entry in directory.directory.entries()? {
        if leaves.len() >= total_leaf_limit {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "legacy scrollback total directory leaf limit exceeded",
            ));
        }
        let entry = entry?;
        leaves.push(PathBuf::from(entry.file_name()));
    }
    leaves.sort();
    let mut candidate_leaf_count = 0usize;
    let mut paired_lock_count = 0usize;
    for leaf in &leaves {
        if paired_scrollback_data_leaf(leaf)
            .is_some_and(|data_leaf| leaves.binary_search(&data_leaf).is_ok())
        {
            paired_lock_count = paired_lock_count.saturating_add(1);
            if paired_lock_count > limits.max_directory_entries {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "legacy scrollback internal lock companion limit exceeded",
                ));
            }
        } else {
            candidate_leaf_count = candidate_leaf_count.saturating_add(1);
            if candidate_leaf_count > limits.max_directory_entries {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "legacy scrollback candidate directory entry limit exceeded",
                ));
            }
        }
    }
    for leaf in leaves {
        let metadata = directory.directory.symlink_metadata(&leaf)?;
        // A completed discard deliberately retains the flock inode. Keep that
        // internal companion out of the candidate list, but only for the one
        // canonical lowercase name shape. It has already consumed one bounded
        // census slot above; uppercase aliases and unrelated leaves remain
        // visible as WrongShape candidates.
        if is_scrollback_lock_filename(&leaf) {
            continue;
        }
        if metadata.is_file() || is_scrollback_filename(&leaf) {
            out.push(classify_in_directory(
                Arc::clone(&directory),
                leaf,
                lock_probe,
                limits,
            ));
        }
    }
    directory.revalidate()?;
    Ok(out)
}

/// Classify one path. Public for callers that already have a
/// `Vec<PathBuf>` and want to reuse the per-file logic without
/// re-walking the directory.
pub fn classify_path(
    path: &Path,
    lock_probe: &impl LockProbe,
    limits: LegacyRecoveryLimits,
) -> OrphanCandidate {
    let limits = match limits.validate() {
        Ok(limits) => limits,
        Err(_) => return unsafe_candidate(path, OrphanUnsafeReason::InvalidLimits),
    };
    if !is_scrollback_filename(path) {
        return OrphanCandidate {
            path: path.to_path_buf(),
            state: OrphanState::WrongShape,
            header: None,
            unsafe_reason: None,
            directory: None,
            leaf: None,
            recovery_lease: None,
            data_identity: None,
        };
    }
    let Some(leaf) = path.file_name().map(PathBuf::from) else {
        return unsafe_candidate(path, OrphanUnsafeReason::NotRegularFile);
    };
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = match PinnedScrollbackDirectory::open(parent_path) {
        Ok(directory) => Arc::new(directory),
        Err(_) => return unsafe_candidate(path, OrphanUnsafeReason::IdentityChanged),
    };
    classify_in_directory(directory, leaf, lock_probe, limits)
}

fn classify_in_directory(
    directory: Arc<PinnedScrollbackDirectory>,
    leaf: PathBuf,
    lock_probe: &impl LockProbe,
    limits: LegacyRecoveryLimits,
) -> OrphanCandidate {
    let path = directory.display_path.join(&leaf);
    if !is_scrollback_filename(&leaf) {
        return OrphanCandidate {
            path,
            state: OrphanState::WrongShape,
            header: None,
            unsafe_reason: None,
            directory: Some(directory),
            leaf: Some(leaf),
            recovery_lease: None,
            data_identity: None,
        };
    }

    let path_metadata = match directory.directory.symlink_metadata(&leaf) {
        Ok(metadata) => metadata,
        Err(_) => return unsafe_candidate(&path, OrphanUnsafeReason::IdentityChanged),
    };
    let directory_metadata = match directory.directory.dir_metadata() {
        Ok(metadata) => metadata,
        Err(_) => return unsafe_candidate(&path, OrphanUnsafeReason::IdentityChanged),
    };
    if let Err(reason) = validate_candidate_metadata(&path_metadata, &directory_metadata) {
        return unsafe_candidate(&path, reason);
    }
    if path_metadata.len() > limits.max_file_bytes {
        return unsafe_candidate(&path, OrphanUnsafeReason::Oversized);
    }

    let lock_path = path.with_extension("bin.lock");
    let recovery_lease = match lock_probe.probe(&lock_path) {
        LockProbeOutcome::Locked => {
            return OrphanCandidate {
                path,
                state: OrphanState::Locked,
                header: None,
                unsafe_reason: None,
                directory: Some(directory),
                leaf: Some(leaf),
                recovery_lease: None,
                data_identity: None,
            };
        }
        LockProbeOutcome::Acquired(lease) => {
            if lease.directory_identity() != directory.identity {
                return unsafe_candidate(&path, OrphanUnsafeReason::IdentityChanged);
            }
            Some(lease)
        }
        LockProbeOutcome::UnlockedWithoutLease => None,
        LockProbeOutcome::Unsafe => {
            return unsafe_candidate(&path, OrphanUnsafeReason::UnsafeLock);
        }
    };

    let (decoded_header, data_identity, classified_file_bytes) =
        match read_header_in_directory(&directory.directory, &leaf) {
            Ok(result) => result,
            Err(reason) => return unsafe_candidate(&path, reason),
        };
    if classified_file_bytes > limits.max_file_bytes {
        return unsafe_candidate(&path, OrphanUnsafeReason::Oversized);
    }
    if directory.revalidate().is_err()
        || recovery_lease
            .as_ref()
            .is_some_and(|lease| lease.revalidate().is_err())
    {
        return unsafe_candidate(&path, OrphanUnsafeReason::IdentityChanged);
    }
    let header = match decoded_header {
        Ok(header) => header,
        Err(error) => {
            return OrphanCandidate {
                path,
                state: OrphanState::Corrupt,
                header: Some(Err(error)),
                unsafe_reason: None,
                directory: Some(directory),
                leaf: Some(leaf),
                recovery_lease,
                data_identity: Some(data_identity),
            };
        }
    };
    if !header_matches_filename(&leaf, &header) {
        return unsafe_candidate(&path, OrphanUnsafeReason::FilenameHeaderMismatch);
    }
    OrphanCandidate {
        path,
        state: OrphanState::Orphaned,
        header: Some(Ok(header)),
        unsafe_reason: None,
        directory: Some(directory),
        leaf: Some(leaf),
        recovery_lease,
        data_identity: Some(data_identity),
    }
}

fn unsafe_candidate(path: &Path, reason: OrphanUnsafeReason) -> OrphanCandidate {
    OrphanCandidate {
        path: path.to_path_buf(),
        state: OrphanState::Unsafe,
        header: None,
        unsafe_reason: Some(reason),
        directory: None,
        leaf: None,
        recovery_lease: None,
        data_identity: None,
    }
}

fn validate_candidate_metadata(
    metadata: &cap_std::fs::Metadata,
    directory_metadata: &cap_std::fs::Metadata,
) -> Result<(), OrphanUnsafeReason> {
    if !metadata.is_file() {
        return Err(OrphanUnsafeReason::NotRegularFile);
    }
    if metadata.nlink() != 1 {
        return Err(OrphanUnsafeReason::HardLinked);
    }
    #[cfg(unix)]
    {
        if cap_std::fs::MetadataExt::mode(metadata) & 0o7177 != 0 {
            return Err(OrphanUnsafeReason::NotPrivate);
        }
        if cap_std::fs::MetadataExt::uid(metadata)
            != cap_std::fs::MetadataExt::uid(directory_metadata)
        {
            return Err(OrphanUnsafeReason::OwnerMismatch);
        }
    }
    #[cfg(not(unix))]
    let _ = directory_metadata;
    Ok(())
}

fn validate_private_pair(
    path_metadata: &cap_std::fs::Metadata,
    handle_metadata: &std::fs::Metadata,
) -> std::io::Result<()> {
    if !path_metadata.is_file()
        || !handle_metadata.is_file()
        || path_metadata.nlink() != 1
        || handle_metadata.nlink() != 1
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "recovery leaf is not one regular file with nlink=1",
        ));
    }
    #[cfg(unix)]
    {
        if cap_std::fs::MetadataExt::mode(path_metadata) & 0o7177 != 0
            || std::os::unix::fs::MetadataExt::mode(handle_metadata) & 0o7177 != 0
            || cap_std::fs::MetadataExt::uid(path_metadata)
                != std::os::unix::fs::MetadataExt::uid(handle_metadata)
        {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "recovery leaf is not private or changed owner",
            ));
        }
    }
    if metadata_identity(path_metadata) != metadata_identity(handle_metadata) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "recovery leaf path and descriptor identities differ",
        ));
    }
    Ok(())
}

fn read_header_in_directory(
    directory: &CapDir,
    leaf: &Path,
) -> Result<
    (
        Result<ScrollbackHeader, HeaderDecodeError>,
        LinearRecordSourceIdentity,
        u64,
    ),
    OrphanUnsafeReason,
> {
    let directory_metadata = directory
        .dir_metadata()
        .map_err(|_| OrphanUnsafeReason::IdentityChanged)?;
    let before = directory
        .symlink_metadata(leaf)
        .map_err(|_| OrphanUnsafeReason::IdentityChanged)?;
    validate_candidate_metadata(&before, &directory_metadata)?;

    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = directory
        .open_with(leaf, &options)
        .map_err(|_| OrphanUnsafeReason::IdentityChanged)?
        .into_std();
    let opened = file
        .metadata()
        .map_err(|_| OrphanUnsafeReason::IdentityChanged)?;
    validate_private_pair(&before, &opened).map_err(|_| OrphanUnsafeReason::IdentityChanged)?;

    let mut bytes = [0u8; HEADER_SIZE];
    let decoded = if file.read_exact(&mut bytes).is_err() {
        let actual = usize::try_from(opened.len())
            .unwrap_or(usize::MAX)
            .min(HEADER_SIZE);
        Err(HeaderDecodeError::Truncated {
            expected: HEADER_SIZE,
            actual,
        })
    } else {
        ScrollbackHeader::decode(&bytes)
    };
    let after = directory
        .symlink_metadata(leaf)
        .map_err(|_| OrphanUnsafeReason::IdentityChanged)?;
    let opened_after = file
        .metadata()
        .map_err(|_| OrphanUnsafeReason::IdentityChanged)?;
    validate_private_pair(&after, &opened_after)
        .map_err(|_| OrphanUnsafeReason::IdentityChanged)?;
    if metadata_identity(&before) != metadata_identity(&after)
        || metadata_identity(&opened) != metadata_identity(&opened_after)
        || metadata_identity(&opened) != metadata_identity(&after)
        || opened.len() != opened_after.len()
    {
        return Err(OrphanUnsafeReason::IdentityChanged);
    }
    Ok((decoded, metadata_identity(&opened), opened.len()))
}

fn header_matches_filename(leaf: &Path, header: &ScrollbackHeader) -> bool {
    let Some(stem) = leaf.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let mut decoded = [0u8; 32];
    hex::decode_to_slice(stem, &mut decoded).is_ok() && decoded == header.pane_uuid
}

fn open_directory_tree_nofollow(path: &Path) -> std::io::Result<CapDir> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "scrollback directory path contains a parent component",
        ));
    }
    let Some(leaf) = path.file_name() else {
        let base = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        return CapDir::open_ambient_dir(base, cap_std::ambient_authority());
    };
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = open_directory_tree_nofollow(parent_path)?;
    parent.open_dir_nofollow(leaf)
}

/// Heuristic: is this filename a scrollback `<pane_uuid>.bin`?
/// Accepts a canonical 64-character lowercase-hex stem (the `pane_uuid` is 32
/// bytes -> 64 hex chars) plus `.bin` extension. Anything else is classified
/// as [`OrphanState::WrongShape`].
fn is_scrollback_filename(path: &Path) -> bool {
    let stem = match path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return false,
    };
    let ext = path.extension().and_then(|s| s.to_str());
    if ext != Some("bin") {
        return false;
    }
    if stem.len() != 64 {
        return false;
    }
    stem.bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Is this the canonical internal lock companion for one scrollback data
/// leaf? Lock leaves count toward the bounded census but are not candidates.
fn is_scrollback_lock_filename(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".bin.lock") else {
        return false;
    };
    stem.len() == 64
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn paired_scrollback_data_leaf(lock_leaf: &Path) -> Option<PathBuf> {
    if !is_scrollback_lock_filename(lock_leaf) {
        return None;
    }
    let name = lock_leaf.file_name()?.to_str()?;
    Some(PathBuf::from(name.strip_suffix(".lock")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback_mmap_format::{FormatVersion, HeaderFlags};
    use std::fs;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ft_5te6x_{label}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn recovery_limits() -> LegacyRecoveryLimits {
        LegacyRecoveryLimits::DEFAULT
    }

    fn complete_snapshot(records: Vec<(RecordKind, Vec<u8>)>) -> LinearRecordSnapshot {
        let payload_bytes = records.iter().fold(0u64, |total, (_, payload)| {
            total + u64::try_from(payload.len()).unwrap()
        });
        LinearRecordSnapshot {
            header: ScrollbackHeader::new([0; 32], 1024, 0),
            records,
            payload_bytes,
            source_identity: LinearRecordSourceIdentity {
                device: 0,
                inode: 0,
            },
            completeness: LinearRecordCompleteness::Complete,
        }
    }

    fn write_private(path: &Path, bytes: &[u8]) {
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    fn write_valid_scrollback(dir: &Path, uuid_byte: u8) -> PathBuf {
        let stem = format!("{uuid_byte:02x}").repeat(32);
        let path = dir.join(format!("{stem}.bin"));
        let header = ScrollbackHeader {
            version: FormatVersion::V1,
            flags: HeaderFlags::empty(),
            capacity_bytes: 1024,
            write_cursor_bytes: 256,
            pane_uuid: [uuid_byte; 32],
            created_at_epoch_ms: 1_000,
            last_msync_at_epoch_ms: 2_000,
            redactions_applied: 3,
            total_bytes_written: 100,
        };
        let bytes = header.encode();
        write_private(&path, &bytes);
        path
    }

    fn write_corrupt_scrollback(dir: &Path, uuid_byte: u8) -> PathBuf {
        let stem = format!("{uuid_byte:02x}").repeat(32);
        let path = dir.join(format!("{stem}.bin"));
        let mut bad = vec![0u8; HEADER_SIZE];
        bad[0..4].copy_from_slice(b"NOPE");
        write_private(&path, &bad);
        path
    }

    #[test]
    fn mmap_replay_plan_preserves_exact_record_payloads() {
        let plan = MmapReplayPlan::from_snapshot(
            complete_snapshot(vec![
                (RecordKind::Text, b"first".to_vec()),
                (RecordKind::Text, b"\nsecond".to_vec()),
                (RecordKind::Csi, b"\x1b[31m".to_vec()),
            ]),
            DEFAULT_REPLAY_CHUNK_BYTES,
            recovery_limits(),
        )
        .unwrap();

        assert_eq!(plan.status(), MmapReplayStatus::Replayed);
        assert_eq!(plan.records_read, 3);
        assert_eq!(plan.records_replayed, 3);
        assert_eq!(plan.bytes_read, "first\nsecond\x1b[31m".len());
        assert_eq!(plan.bytes_replayed, plan.bytes_read);
        let replayed = plan
            .chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>();
        assert_eq!(replayed, "first\nsecond\x1b[31m");
        assert_eq!(plan.skipped, [] as [scrollback_mmap_recovery::MmapReplaySkippedRecord; 0]);
    }

    #[test]
    fn mmap_replay_plan_reports_invalid_utf8_without_lossy_replay() {
        let plan = MmapReplayPlan::from_snapshot(
            complete_snapshot(vec![
                (RecordKind::Text, b"ok".to_vec()),
                (RecordKind::Text, vec![0xff, b'x']),
            ]),
            DEFAULT_REPLAY_CHUNK_BYTES,
            recovery_limits(),
        )
        .unwrap();

        assert_eq!(plan.status(), MmapReplayStatus::Partial);
        assert_eq!(plan.records_read, 2);
        assert_eq!(plan.records_replayed, 1);
        assert_eq!(plan.bytes_read, 4);
        assert_eq!(plan.bytes_replayed, 2);
        assert_eq!(plan.chunks.len(), 1);
        assert_eq!(plan.chunks[0].text, "ok");
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].record_index, 1);
        assert_eq!(plan.skipped[0].reason.as_str(), "invalid_utf8");
    }

    #[test]
    fn mmap_replay_plan_chunks_on_utf8_boundaries() {
        let plan = MmapReplayPlan::from_snapshot(
            complete_snapshot(vec![(RecordKind::Text, "aébc".as_bytes().to_vec())]),
            2,
            recovery_limits(),
        )
        .unwrap();

        let replayed = plan
            .chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>();
        assert_eq!(plan.status(), MmapReplayStatus::Replayed);
        assert_eq!(replayed, "aébc");
        assert!(
            plan.chunks
                .iter()
                .all(|chunk| std::str::from_utf8(chunk.text.as_bytes()).is_ok())
        );
    }

    #[test]
    fn scan_empty_dir_returns_empty_vec() {
        let dir = temp_dir("empty");
        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn scan_decodes_valid_scrollback_as_orphaned() {
        let dir = temp_dir("valid");
        write_valid_scrollback(&dir, 0xAB);
        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Orphaned);
        let h = out[0].header_ok().expect("header decoded");
        assert_eq!(h.pane_uuid[0], 0xAB);
        assert_eq!(h.write_cursor_bytes, 256);
    }

    #[test]
    fn scan_marks_locked_files_correctly() {
        let dir = temp_dir("locked");
        let p = write_valid_scrollback(&dir, 0xCD);
        let lock_p = p.with_extension("bin.lock");
        let out = scan_orphans(
            &dir,
            &|probe_path: &Path| probe_path == lock_p,
            recovery_limits(),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Locked);
        // A live writer's data file is never inspected without its lock.
        assert!(out[0].header_ok().is_none());
    }

    #[test]
    fn scan_marks_corrupt_header_with_decode_error() {
        let dir = temp_dir("corrupt");
        // Write a 64-char hex stem with bad magic bytes.
        let stem = "ee".repeat(32);
        let path = dir.join(format!("{stem}.bin"));
        let mut bad = vec![0u8; HEADER_SIZE];
        bad[0..4].copy_from_slice(b"XXXX"); // bad magic
        write_private(&path, &bad);

        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Corrupt);
        let reason = out[0].corrupt_reason().expect("decode error attached");
        assert!(matches!(reason, HeaderDecodeError::BadMagic { .. }));
    }

    #[test]
    fn scan_marks_truncated_file_as_corrupt() {
        let dir = temp_dir("trunc");
        let stem = "11".repeat(32);
        let path = dir.join(format!("{stem}.bin"));
        write_private(&path, &[0u8; 50]); // shorter than HEADER_SIZE

        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Corrupt);
        let reason = out[0].corrupt_reason().expect("decode error attached");
        assert!(matches!(reason, HeaderDecodeError::Truncated { .. }));
    }

    #[test]
    fn scan_skips_wrong_shape_files() {
        let dir = temp_dir("wrong_shape");
        // Drop a non-matching file.
        fs::write(dir.join("README.md"), b"not a scrollback").unwrap();
        // Drop a `.txt` file too.
        fs::write(dir.join("notes.txt"), b"unrelated").unwrap();
        // Drop a scrollback-shaped file alongside.
        write_valid_scrollback(&dir, 0xFF);

        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert_eq!(out.len(), 3);
        let states: Vec<&OrphanState> = out.iter().map(|c| &c.state).collect();
        // Sorted by path, so `README.md` (R) < `<hex>.bin` (f) < `notes.txt` (n).
        // Sort order: ASCII compares uppercase < lowercase, so 'R' < 'f' < 'n'.
        assert!(states.contains(&&OrphanState::Orphaned));
        assert_eq!(
            states
                .iter()
                .filter(|s| ***s == OrphanState::WrongShape)
                .count(),
            2
        );
    }

    #[test]
    fn scan_classifies_short_stem_as_wrong_shape() {
        let dir = temp_dir("short");
        // .bin extension but stem is only 8 chars, not 64.
        fs::write(dir.join("abcd1234.bin"), [0u8; HEADER_SIZE]).unwrap();
        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::WrongShape);
    }

    #[test]
    fn scan_classifies_non_hex_stem_as_wrong_shape() {
        let dir = temp_dir("nonhex");
        // 64 chars but contains non-hex.
        let stem = "z".repeat(64);
        fs::write(dir.join(format!("{stem}.bin")), [0u8; HEADER_SIZE]).unwrap();
        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::WrongShape);
    }

    #[test]
    fn scan_classifies_uppercase_hex_stem_as_wrong_shape() {
        let dir = temp_dir("uppercase-hex");
        let stem = "AB".repeat(32);
        fs::write(dir.join(format!("{stem}.bin")), [0u8; HEADER_SIZE]).unwrap();

        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::WrongShape);
    }

    #[test]
    fn scan_filters_only_canonical_lowercase_lock_companion_names() {
        let dir = temp_dir("canonical-lock-filter");
        let lowercase_lock = dir.join(format!("{}.bin.lock", "ab".repeat(32)));
        let uppercase_lock = dir.join(format!("{}.bin.lock", "AB".repeat(32)));
        write_private(&lowercase_lock, &[]);
        write_private(&uppercase_lock, &[]);

        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, uppercase_lock);
        assert_eq!(out[0].state, OrphanState::WrongShape);

        let one_entry_limit = LegacyRecoveryLimits {
            max_directory_entries: 1,
            ..recovery_limits()
        };
        assert!(
            scan_orphans(&dir, &AlwaysOrphaned, one_entry_limit).is_err(),
            "filtered lock companions must still consume bounded census slots"
        );
    }

    #[test]
    fn scan_classifies_wrong_extension_as_wrong_shape() {
        let dir = temp_dir("wrongext");
        // 64-hex stem but `.dat` extension instead of `.bin`.
        let stem = "00".repeat(32);
        fs::write(dir.join(format!("{stem}.dat")), [0u8; HEADER_SIZE]).unwrap();
        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::WrongShape);
    }

    #[test]
    fn scan_handles_multiple_orphans_in_sorted_order() {
        let dir = temp_dir("multi");
        write_valid_scrollback(&dir, 0x00);
        write_valid_scrollback(&dir, 0x11);
        write_valid_scrollback(&dir, 0x22);
        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|c| c.state == OrphanState::Orphaned));
        // Sorted by path → uuid bytes 0x00 < 0x11 < 0x22.
        assert_eq!(out[0].header_ok().unwrap().pane_uuid[0], 0x00);
        assert_eq!(out[1].header_ok().unwrap().pane_uuid[0], 0x11);
        assert_eq!(out[2].header_ok().unwrap().pane_uuid[0], 0x22);
    }

    #[test]
    fn classify_path_works_standalone_without_directory_walk() {
        let dir = temp_dir("standalone");
        let p = write_valid_scrollback(&dir, 0x55);
        let candidate = classify_path(&p, &AlwaysOrphaned, recovery_limits());
        assert_eq!(candidate.state, OrphanState::Orphaned);
        assert!(candidate.header_ok().is_some());
    }

    #[test]
    fn scan_returns_io_error_on_missing_directory() {
        let dir = std::env::temp_dir().join("ft_5te6x_does_not_exist_anywhere");
        let _ = fs::remove_dir_all(&dir);
        let result = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits());
        assert!(result.is_err());
    }

    #[test]
    fn lock_probe_closure_form_works() {
        let dir = temp_dir("closure_probe");
        let p = write_valid_scrollback(&dir, 0xEE);
        let expected_lock = p.with_extension("bin.lock");
        let probe = |path: &Path| path == expected_lock;
        let out = scan_orphans(&dir, &probe, recovery_limits()).unwrap();
        assert_eq!(out[0].state, OrphanState::Locked);
    }

    #[test]
    fn always_orphaned_probe_treats_every_file_as_orphan() {
        let dir = temp_dir("always_orphaned");
        write_valid_scrollback(&dir, 0x99);
        let out = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        assert_eq!(out[0].state, OrphanState::Orphaned);
    }

    #[test]
    fn flock_lock_probe_treats_missing_lock_as_orphan() {
        let dir = temp_dir("flock_missing");
        write_valid_scrollback(&dir, 0x9A);

        let out = scan_orphans(&dir, &FlockLockProbe, recovery_limits()).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Orphaned);
    }

    #[test]
    fn production_probe_repeated_scan_does_not_exhaust_its_own_lock_census() {
        let dir = temp_dir("flock_repeated_census");
        for uuid_byte in 0..65u8 {
            write_valid_scrollback(&dir, uuid_byte);
        }

        let first = scan_orphans(&dir, &FlockLockProbe, recovery_limits())
            .expect("first production scan with missing lock companions");
        let first_facts: Vec<(PathBuf, OrphanState)> = first
            .iter()
            .map(|candidate| (candidate.path.clone(), candidate.state.clone()))
            .collect();
        assert_eq!(first_facts.len(), 65);
        assert!(
            first_facts
                .iter()
                .all(|(_, state)| *state == OrphanState::Orphaned)
        );
        drop(first);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            130,
            "the production probe creates one retained lock companion per data leaf"
        );

        let second = scan_orphans(&dir, &FlockLockProbe, recovery_limits())
            .expect("second production scan must admit paired internal lock companions");
        let second_facts: Vec<(PathBuf, OrphanState)> = second
            .iter()
            .map(|candidate| (candidate.path.clone(), candidate.state.clone()))
            .collect();
        assert_eq!(second_facts, first_facts);
    }

    #[test]
    fn flock_lock_probe_detects_live_lock_holder() {
        let dir = temp_dir("flock_held");
        let path = write_valid_scrollback(&dir, 0x9B);
        let lock_path = path.with_extension("bin.lock");
        let mut lock_options = fs::OpenOptions::new();
        lock_options
            .create(true)
            .truncate(false)
            .read(true)
            .write(true);
        #[cfg(unix)]
        lock_options.mode(0o600);
        let lock_file = lock_options.open(&lock_path).unwrap();
        lock_file.lock_exclusive().unwrap();

        let out = scan_orphans(&dir, &FlockLockProbe, recovery_limits()).unwrap();
        let candidate = out
            .iter()
            .find(|candidate| candidate.path == path)
            .expect("scrollback candidate present");

        assert_eq!(candidate.state, OrphanState::Locked);
        assert!(candidate.header_ok().is_none());
        FileExt::unlock(&lock_file).unwrap();
    }

    #[test]
    fn scan_fails_closed_at_directory_census_limit() {
        let dir = temp_dir("bounded-census");
        write_private(&dir.join("one.txt"), b"one");
        write_private(&dir.join("two.txt"), b"two");
        let limits = LegacyRecoveryLimits {
            max_directory_entries: 1,
            ..recovery_limits()
        };

        let error = scan_orphans(&dir, &AlwaysOrphaned, limits).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn shaped_symlink_and_symlinked_ancestor_are_never_followed() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("nofollow");
        let target = write_valid_scrollback(&dir, 0x31);
        let symlink_leaf = dir.join(format!("{}.bin", "32".repeat(32)));
        symlink(&target, &symlink_leaf).unwrap();
        let candidates = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        let symlink_candidate = candidates
            .iter()
            .find(|candidate| candidate.path == symlink_leaf)
            .unwrap();
        assert_eq!(symlink_candidate.state, OrphanState::Unsafe);
        assert_eq!(
            symlink_candidate.unsafe_reason(),
            Some(OrphanUnsafeReason::NotRegularFile)
        );

        let linked_parent = dir.with_extension("linked-parent");
        symlink(&dir, &linked_parent).unwrap();
        let linked_path = linked_parent.join(target.file_name().unwrap());
        let candidate = classify_path(&linked_path, &AlwaysOrphaned, recovery_limits());
        assert_eq!(candidate.state, OrphanState::Unsafe);
        assert_eq!(
            candidate.unsafe_reason(),
            Some(OrphanUnsafeReason::IdentityChanged)
        );
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_nonprivate_and_mismatched_candidates_fail_closed() {
        let dir = temp_dir("private-identity");

        let hardlinked = write_valid_scrollback(&dir, 0x41);
        fs::hard_link(&hardlinked, dir.join("hardlink-copy")).unwrap();
        let candidate = classify_path(&hardlinked, &AlwaysOrphaned, recovery_limits());
        assert_eq!(candidate.state, OrphanState::Unsafe);
        assert_eq!(
            candidate.unsafe_reason(),
            Some(OrphanUnsafeReason::HardLinked)
        );

        let nonprivate = write_valid_scrollback(&dir, 0x42);
        fs::set_permissions(&nonprivate, fs::Permissions::from_mode(0o644)).unwrap();
        let candidate = classify_path(&nonprivate, &AlwaysOrphaned, recovery_limits());
        assert_eq!(candidate.state, OrphanState::Unsafe);
        assert_eq!(
            candidate.unsafe_reason(),
            Some(OrphanUnsafeReason::NotPrivate)
        );

        let mismatched = write_valid_scrollback(&dir, 0x43);
        let mismatched_name = dir.join(format!("{}.bin", "44".repeat(32)));
        fs::rename(&mismatched, &mismatched_name).unwrap();
        let candidate = classify_path(&mismatched_name, &AlwaysOrphaned, recovery_limits());
        assert_eq!(candidate.state, OrphanState::Unsafe);
        assert_eq!(
            candidate.unsafe_reason(),
            Some(OrphanUnsafeReason::FilenameHeaderMismatch)
        );
    }

    #[test]
    fn oversized_candidate_is_rejected_before_header_read() {
        let dir = temp_dir("oversized");
        let path = write_valid_scrollback(&dir, 0x51);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(HEADER_SIZE as u64 + 2)
            .unwrap();
        let limits = LegacyRecoveryLimits {
            max_file_bytes: HEADER_SIZE as u64 + 1,
            max_records: 1,
            max_replay_chunks: 1,
            max_payload_bytes: 1,
            max_transcript_bytes: 1,
            ..recovery_limits()
        };

        let candidate = classify_path(&path, &AlwaysOrphaned, limits);

        assert_eq!(candidate.state, OrphanState::Unsafe);
        assert_eq!(
            candidate.unsafe_reason(),
            Some(OrphanUnsafeReason::Oversized)
        );
    }

    #[test]
    fn production_candidate_holds_lock_through_bounded_read() {
        let dir = temp_dir("held-through-read");
        let path = write_valid_scrollback(&dir, 0x61);
        let candidate = scan_orphans(&dir, &FlockLockProbe, recovery_limits())
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.path == path)
            .unwrap();
        assert_eq!(candidate.state, OrphanState::Orphaned);

        let competing = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_extension("bin.lock"))
            .unwrap();
        assert_eq!(
            competing.try_lock_exclusive().unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        let snapshot = candidate.read_records(recovery_limits()).unwrap();
        assert_eq!(snapshot.records, [] as [(scrollback_mmap_format::RecordKind, std::vec::Vec<u8>); 0]);
        assert_eq!(snapshot.header.pane_uuid, [0x61; 32]);
        drop(candidate);
        competing.try_lock_exclusive().unwrap();
        FileExt::unlock(&competing).unwrap();
    }

    #[test]
    fn production_candidate_rejects_data_replacement_after_scan() {
        let dir = temp_dir("replacement-after-scan");
        let path = write_valid_scrollback(&dir, 0x66);
        let candidate = scan_orphans(&dir, &FlockLockProbe, recovery_limits())
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.path == path)
            .unwrap();
        let original = dir.join("original-after-scan.bin");
        fs::rename(&path, &original).unwrap();
        let replacement_bytes = fs::read(&original).unwrap();
        write_private(&path, &replacement_bytes);

        let error = candidate.read_records(recovery_limits()).unwrap_err();

        assert!(matches!(
            error,
            MmapScrollbackError::UnsafeReadSource { .. }
        ));
        assert!(path.exists(), "replacement must remain untouched");
    }

    #[test]
    fn leased_discard_removes_only_data_and_retains_reusable_lock_leaf() {
        let dir = temp_dir("leased-discard");
        let path = write_valid_scrollback(&dir, 0x67);
        let lock_path = path.with_extension("bin.lock");
        let candidate = scan_orphans(&dir, &FlockLockProbe, recovery_limits())
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.path == path)
            .unwrap();
        let reusable_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert_eq!(
            reusable_lock.try_lock_exclusive().unwrap_err().kind(),
            ErrorKind::WouldBlock
        );

        let receipt = candidate.discard().expect("identity-checked discard");

        assert_eq!(receipt.data_path, path);
        assert_eq!(receipt.retained_lock_path, lock_path);
        assert!(receipt.directory_synced);
        assert!(!path.exists());
        assert!(lock_path.is_file(), "lock inode is deliberately retained");
        reusable_lock.try_lock_exclusive().unwrap();
        FileExt::unlock(&reusable_lock).unwrap();

        let after = scan_orphans(&dir, &FlockLockProbe, recovery_limits()).unwrap();
        assert!(
            after.is_empty(),
            "the retained canonical lock is internal, not a recovery candidate"
        );
    }

    #[test]
    fn leased_discard_rejects_a_replaced_data_leaf_without_deleting_it() {
        let dir = temp_dir("discard-replacement");
        let path = write_valid_scrollback(&dir, 0x68);
        let candidate = scan_orphans(&dir, &FlockLockProbe, recovery_limits())
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.path == path)
            .unwrap();
        let original = dir.join("discard-original.bin");
        fs::rename(&path, &original).unwrap();
        let replacement = fs::read(&original).unwrap();
        write_private(&path, &replacement);

        let error = candidate.discard().unwrap_err();

        assert!(matches!(error, LegacyRecoveryDiscardError::Unsafe { .. }));
        assert_eq!(fs::read(&path).unwrap(), replacement);
        assert!(original.exists());
    }

    #[test]
    fn leased_discard_rejects_a_mutated_public_display_path() {
        let dir = temp_dir("discard-mutated-display-path");
        let path = write_valid_scrollback(&dir, 0x6b);
        let mut candidate = scan_orphans(&dir, &FlockLockProbe, recovery_limits())
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.path == path)
            .unwrap();
        let claimed_path = dir.join(format!("{}.bin", "6c".repeat(32)));
        candidate.path = claimed_path.clone();

        let error = candidate.discard().unwrap_err();

        assert!(matches!(error, LegacyRecoveryDiscardError::Unsafe { .. }));
        assert!(
            path.is_file(),
            "the pinned data capability must remain intact"
        );
        assert!(!claimed_path.exists());
    }

    #[test]
    fn unleased_candidate_cannot_cross_discard_boundary() {
        let dir = temp_dir("unleased-discard");
        let path = write_valid_scrollback(&dir, 0x69);
        let candidate = classify_path(&path, &AlwaysOrphaned, recovery_limits());

        let error = candidate.discard().unwrap_err();

        assert!(matches!(error, LegacyRecoveryDiscardError::Unsafe { .. }));
        assert!(path.exists());
    }

    #[test]
    fn leased_corrupt_candidate_can_be_discarded_by_bound_identity() {
        let dir = temp_dir("leased-corrupt-discard");
        let path = write_corrupt_scrollback(&dir, 0x6a);
        let candidate = scan_orphans(&dir, &FlockLockProbe, recovery_limits())
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.path == path)
            .unwrap();
        assert_eq!(candidate.state, OrphanState::Corrupt);

        let receipt = candidate
            .discard()
            .expect("discard leased corrupt identity");

        assert_eq!(receipt.data_path, path);
        assert!(!path.exists());
        assert!(receipt.retained_lock_path.is_file());
    }

    #[test]
    fn unleased_test_probe_cannot_cross_recovery_read_boundary() {
        let dir = temp_dir("unleased-read");
        let path = write_valid_scrollback(&dir, 0x62);
        let candidate = classify_path(&path, &AlwaysOrphaned, recovery_limits());

        let error = candidate.read_records(recovery_limits()).unwrap_err();

        assert!(matches!(
            error,
            MmapScrollbackError::UnsafeReadSource { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_lock_symlink_is_not_treated_as_an_orphan() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("unsafe-lock");
        let path = write_valid_scrollback(&dir, 0x63);
        let target = dir.join("lock-target");
        write_private(&target, b"");
        symlink(&target, path.with_extension("bin.lock")).unwrap();

        let candidate = scan_orphans(&dir, &FlockLockProbe, recovery_limits())
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.path == path)
            .unwrap();

        assert_eq!(candidate.state, OrphanState::Unsafe);
        assert_eq!(
            candidate.unsafe_reason(),
            Some(OrphanUnsafeReason::UnsafeLock)
        );
    }

    #[cfg(unix)]
    #[test]
    fn nonprivate_and_hardlinked_lock_files_are_unsafe() {
        let dir = temp_dir("unsafe-lock-metadata");
        let nonprivate_data = write_valid_scrollback(&dir, 0x64);
        let nonprivate_lock = nonprivate_data.with_extension("bin.lock");
        write_private(&nonprivate_lock, b"");
        fs::set_permissions(&nonprivate_lock, fs::Permissions::from_mode(0o644)).unwrap();
        let candidate = classify_path(&nonprivate_data, &FlockLockProbe, recovery_limits());
        assert_eq!(candidate.state, OrphanState::Unsafe);
        assert_eq!(
            candidate.unsafe_reason(),
            Some(OrphanUnsafeReason::UnsafeLock)
        );

        let hardlinked_data = write_valid_scrollback(&dir, 0x65);
        let hardlinked_lock = hardlinked_data.with_extension("bin.lock");
        write_private(&hardlinked_lock, b"");
        fs::hard_link(&hardlinked_lock, dir.join("lock-hardlink-copy")).unwrap();
        let candidate = classify_path(&hardlinked_data, &FlockLockProbe, recovery_limits());
        assert_eq!(candidate.state, OrphanState::Unsafe);
        assert_eq!(
            candidate.unsafe_reason(),
            Some(OrphanUnsafeReason::UnsafeLock)
        );
    }

    #[cfg(unix)]
    #[test]
    fn scanner_rejects_and_does_not_mutate_a_nonprivate_final_directory() {
        let dir = temp_dir("nonprivate-directory");
        write_valid_scrollback(&dir, 0x6b);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        let error = scan_orphans(&dir, &FlockLockProbe, recovery_limits()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("not owner-private"));
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
            0o755,
            "scanner is read-only and must not perform writer migration"
        );
    }

    #[test]
    fn replay_plan_enforces_record_payload_transcript_and_chunk_limits() {
        let limits = LegacyRecoveryLimits {
            max_records: 1,
            max_replay_chunks: 1,
            max_payload_bytes: 4,
            max_transcript_bytes: 4,
            ..recovery_limits()
        };
        assert!(matches!(
            MmapReplayPlan::from_snapshot(
                complete_snapshot(vec![
                    (RecordKind::Text, b"a".to_vec()),
                    (RecordKind::Text, b"b".to_vec()),
                ]),
                4,
                limits,
            ),
            Err(MmapReplayPlanError::LimitExceeded {
                limit_name: "records",
                ..
            })
        ));
        assert!(matches!(
            MmapReplayPlan::from_snapshot(
                complete_snapshot(vec![(RecordKind::Text, b"abcde".to_vec())]),
                4,
                limits,
            ),
            Err(MmapReplayPlanError::LimitExceeded {
                limit_name: "payload_bytes",
                ..
            })
        ));
        assert!(matches!(
            MmapReplayPlan::from_snapshot(
                complete_snapshot(vec![(RecordKind::Text, b"abcd".to_vec())]),
                2,
                limits,
            ),
            Err(MmapReplayPlanError::LimitExceeded {
                limit_name: "replay_chunks",
                ..
            })
        ));

        let transcript_limits = LegacyRecoveryLimits {
            max_payload_bytes: 8,
            max_transcript_bytes: 4,
            ..limits
        };
        assert!(matches!(
            MmapReplayPlan::from_snapshot(
                complete_snapshot(vec![(RecordKind::Text, b"abcde".to_vec())]),
                8,
                transcript_limits,
            ),
            Err(MmapReplayPlanError::LimitExceeded {
                limit_name: "transcript_bytes",
                ..
            })
        ));
    }

    #[test]
    fn replay_plan_never_reports_a_salvaged_prefix_as_complete() {
        let mut snapshot = complete_snapshot(vec![(RecordKind::Text, b"salvaged".to_vec())]);
        snapshot.completeness = LinearRecordCompleteness::Incomplete {
            decoded_cursor_bytes: 16,
            declared_cursor_bytes: 64,
            reason: crate::scrollback_mmap_writer::LinearRecordTerminalReason::PhysicalRecordPayloadTruncated,
        };

        let plan =
            MmapReplayPlan::from_snapshot(snapshot, DEFAULT_REPLAY_CHUNK_BYTES, recovery_limits())
                .unwrap();

        assert_eq!(plan.status(), MmapReplayStatus::Partial);
        assert!(matches!(
            plan.source_completeness,
            LinearRecordCompleteness::Incomplete {
                decoded_cursor_bytes: 16,
                declared_cursor_bytes: 64,
                ..
            }
        ));
    }

    #[test]
    fn picker_filters_wrong_shape_and_builds_accessible_rows() {
        let dir = temp_dir("picker_rows");
        fs::write(dir.join("notes.txt"), b"unrelated").unwrap();
        let orphan_path = write_valid_scrollback(&dir, 0x10);
        let corrupt_path = write_corrupt_scrollback(&dir, 0x20);
        let candidates = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();

        let picker = OrphanPickerState::new(&candidates, RecoveryAction::Recover, 3_000);

        assert_eq!(picker.rows().len(), 2);
        assert_eq!(picker.highlighted(), Some(0));
        assert_eq!(picker.rows()[0].path, orphan_path);
        assert_eq!(picker.rows()[0].badge, OrphanPickerBadge::Orphaned);
        assert!(picker.rows()[0].selectable);
        assert!(
            picker.rows()[0]
                .accessibility_label
                .contains("1010101010101010")
        );
        assert!(
            picker.rows()[0]
                .accessibility_label
                .contains("100 bytes written")
        );
        assert_eq!(picker.rows()[1].path, corrupt_path);
        assert_eq!(picker.rows()[1].badge, OrphanPickerBadge::Corrupt);
        assert!(!picker.rows()[1].selectable);
        assert!(
            picker.rows()[1]
                .accessibility_label
                .contains("corrupt, disabled")
        );
    }

    #[test]
    fn picker_keyboard_toggle_and_confirm_recovery_decisions() {
        let dir = temp_dir("picker_keyboard");
        let first = write_valid_scrollback(&dir, 0x01);
        let second = write_valid_scrollback(&dir, 0x02);
        let candidates = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        let mut picker = OrphanPickerState::new(&candidates, RecoveryAction::Recover, 3_000);

        assert_eq!(
            picker.handle_key(OrphanPickerKey::Toggle),
            OrphanPickerOutcome::Pending
        );
        assert!(picker.rows()[0].selected);
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Down),
            OrphanPickerOutcome::Pending
        );
        assert_eq!(picker.highlighted(), Some(1));
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Confirm),
            OrphanPickerOutcome::Confirmed(vec![
                RecoveryDecision::Recover(first),
                RecoveryDecision::Skip(second),
            ])
        );
    }

    #[test]
    fn picker_locked_rows_are_visible_but_not_selectable() {
        let dir = temp_dir("picker_locked");
        let locked = write_valid_scrollback(&dir, 0x33);
        let lock_p = locked.with_extension("bin.lock");
        let candidates = scan_orphans(
            &dir,
            &|probe_path: &Path| probe_path == lock_p,
            recovery_limits(),
        )
        .unwrap();
        let mut picker = OrphanPickerState::new(&candidates, RecoveryAction::Recover, 3_000);

        assert_eq!(picker.rows().len(), 1);
        assert_eq!(picker.rows()[0].badge, OrphanPickerBadge::Locked);
        assert!(!picker.rows()[0].selectable);
        picker.toggle_highlighted();
        assert!(!picker.rows()[0].selected);
        assert_eq!(picker.confirm(), vec![RecoveryDecision::Skip(locked)]);
    }

    #[test]
    fn picker_discard_mode_can_select_corrupt_candidates() {
        let dir = temp_dir("picker_discard_corrupt");
        let corrupt = write_corrupt_scrollback(&dir, 0x44);
        let candidates = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        let mut picker = OrphanPickerState::new(&candidates, RecoveryAction::Discard, 3_000);

        assert_eq!(picker.rows()[0].badge, OrphanPickerBadge::Corrupt);
        assert!(picker.rows()[0].selectable);
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Toggle),
            OrphanPickerOutcome::Pending
        );
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Confirm),
            OrphanPickerOutcome::Confirmed(vec![RecoveryDecision::Discard(corrupt)])
        );
    }

    #[test]
    fn picker_cancel_reports_cancelled_without_mutating_selection() {
        let dir = temp_dir("picker_cancel");
        write_valid_scrollback(&dir, 0x77);
        let candidates = scan_orphans(&dir, &AlwaysOrphaned, recovery_limits()).unwrap();
        let mut picker = OrphanPickerState::new(&candidates, RecoveryAction::Recover, 3_000);

        picker.toggle_highlighted();
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Cancel),
            OrphanPickerOutcome::Cancelled
        );
        assert!(picker.rows()[0].selected);
    }
}
