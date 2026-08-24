//! Crash-safe scrollback writer over the integrity-checked v2 ring profile.
//!
//! This module intentionally keeps the public contract at the format
//! boundary: one per-pane `.bin` file plus a sidecar `.bin.lock`, dual
//! integrity-checked state slots and records, redaction-before-write,
//! bounded capacity, and explicit sync cadence. Legacy v1 leaves are forensic
//! read-only inputs and are never migrated in place. The crate forbids unsafe
//! code, so this implementation uses positional file writes and `sync_data`
//! instead of calling platform mmap APIs directly.

use crate::redactor::{BytesRedactionEvidence, RedactionResult, StreamingRedactor};
use crate::scrollback_mmap_format::{
    HEADER_SIZE, HeaderFlags, RECORD_HEADER_SIZE, RecordHeader, RecordKind, ScrollbackHeader,
};
use cap_fs_ext::{DirExt as _, FollowSymlinks, MetadataExt as _, OpenOptionsFollowExt as _};
use cap_std::fs::{Dir as CapDir, OpenOptions as CapOpenOptions};
use fs2::FileExt;
use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_CAP_BYTES: u64 = 50 * 1024 * 1024;
pub const HARD_MAX_LINEAR_RECORD_FILE_BYTES: u64 = 1024 * 1024 * 1024;
pub const HARD_MAX_LINEAR_RECORDS: usize = 1_048_576;
pub const HARD_MAX_LINEAR_RECORD_PAYLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CAP_BYTES: u64 = HARD_MAX_LINEAR_RECORD_FILE_BYTES - HEADER_SIZE as u64;
const DEFAULT_SYNC_EVERY_APPENDS: u64 = 64;
const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_millis(250);

// Fresh files use an integrity-checked v2 ring profile inside the existing
// 256-byte envelope. Keeping the outer `FormatVersion::V1` value lets the
// bounded orphan census decode immutable identity/telemetry without teaching
// that scanner how to replay records; the dedicated reader below requires this
// flag and validates the v2 state slots and record digests before returning
// bytes.
const V2_RING_FLAG: u16 = 0x8000;
const V2_STATE_SLOT_MAGIC: [u8; 4] = *b"FTS2";
const V2_STATE_SLOT_VERSION: u16 = 2;
const V2_STATE_SLOT_SIZE: usize = 64;
const V2_STATE_SLOT_COUNT: usize = 2;
const V2_STATE_SLOTS_OFFSET: usize = 88;
const V2_STATE_CHECKSUM_OFFSET: usize = 48;
const V2_STATE_CHECKSUM_BYTES: usize = 16;
const V2_RECORD_MAGIC: [u8; 4] = *b"FTR2";
/// Fixed authenticated record-header bytes used by fresh v2 ring files.
///
/// This is public so external conformance/property tests can size a
/// deliberately non-wrapping ring without encoding the retired v1 overhead.
pub const V2_RECORD_HEADER_SIZE: usize = 64;
const V2_RECORD_DIGEST_OFFSET: usize = 32;
const V2_RECORD_DIGEST_BYTES: usize = 32;
const V2_STATE_HASH_DOMAIN: &[u8] = b"frankenterm.scrollback.v2.state\0";
const V2_RECORD_HASH_DOMAIN: &[u8] = b"frankenterm.scrollback.v2.record\0";

const _: () = assert!(
    V2_STATE_SLOTS_OFFSET + V2_STATE_SLOT_SIZE * V2_STATE_SLOT_COUNT <= HEADER_SIZE
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct V2RingState {
    slot_epoch: u64,
    head: u32,
    tail: u32,
    wrap_at: u32,
    generation: u64,
    next_sequence: u64,
    record_count: u32,
}

impl V2RingState {
    fn fresh(capacity_bytes: u64) -> Result<Self, MmapScrollbackError> {
        Ok(Self {
            slot_epoch: 1,
            head: 0,
            tail: 0,
            wrap_at: u32::try_from(capacity_bytes).map_err(|_| {
                MmapScrollbackError::CapTooLarge {
                    maximum: u64::from(u32::MAX),
                    actual: capacity_bytes,
                }
            })?,
            generation: 0,
            next_sequence: 0,
            record_count: 0,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct V2RecordMeta {
    total_len: u32,
    payload_len: u32,
    generation: u64,
    sequence: u64,
}

#[derive(Debug)]
struct OpenedWriterFile {
    file: File,
    identity: LinearRecordSourceIdentity,
    created: bool,
}

/// Caller-owned resource envelope for decoding a legacy mmap scrollback file.
///
/// Recovery is forensic and fail-closed: reaching any limit returns an error;
/// it never presents a truncated prefix as a complete export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinearRecordReadLimits {
    pub max_file_bytes: u64,
    pub max_records: usize,
    pub max_payload_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearRecordSnapshot {
    pub header: ScrollbackHeader,
    pub records: Vec<(RecordKind, Vec<u8>)>,
    pub payload_bytes: u64,
    pub source_identity: LinearRecordSourceIdentity,
    /// Whether every byte in the header-declared committed prefix was decoded.
    /// A crash may leave a useful, fully decoded prefix followed by a torn
    /// record; callers must preserve this status instead of presenting the
    /// salvaged prefix as a complete export.
    pub completeness: LinearRecordCompleteness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinearRecordSourceIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearRecordCompleteness {
    Complete,
    Incomplete {
        decoded_cursor_bytes: u64,
        declared_cursor_bytes: u64,
        reason: LinearRecordTerminalReason,
    },
}

impl LinearRecordCompleteness {
    #[must_use]
    pub const fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Incomplete { .. } => "incomplete",
        }
    }
}

/// Why record decoding stopped before the header-declared committed cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearRecordTerminalReason {
    DeclaredTailTooShortForHeader,
    PhysicalRecordHeaderTruncated,
    ZeroFilledRecordHeader,
    RecordPayloadPastDeclaredCursor,
    PhysicalRecordPayloadTruncated,
    /// A v1 writer has overwritten the beginning of its body, but v1 stores
    /// neither the oldest-record offset nor a generation. Physical order is
    /// therefore not logical order and cannot be exported exactly.
    LegacyWrappedOrderUnknown,
    /// A newer v2 state publication or one of its authenticated records was
    /// torn/corrupt, so recovery fell back to an older valid state slot.
    V2NewerStateInvalid,
}

impl LinearRecordTerminalReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeclaredTailTooShortForHeader => "declared_tail_too_short_for_header",
            Self::PhysicalRecordHeaderTruncated => "physical_record_header_truncated",
            Self::ZeroFilledRecordHeader => "zero_filled_record_header",
            Self::RecordPayloadPastDeclaredCursor => "record_payload_past_declared_cursor",
            Self::PhysicalRecordPayloadTruncated => "physical_record_payload_truncated",
            Self::LegacyWrappedOrderUnknown => "legacy_wrapped_order_unknown",
            Self::V2NewerStateInvalid => "v2_newer_state_invalid",
        }
    }
}

impl LinearRecordReadLimits {
    pub fn validate(self) -> Result<Self, MmapScrollbackError> {
        if self.max_file_bytes < HEADER_SIZE as u64 {
            return Err(MmapScrollbackError::InvalidReadLimit {
                limit_name: "file_bytes",
            });
        }
        if self.max_records == 0 {
            return Err(MmapScrollbackError::InvalidReadLimit {
                limit_name: "records",
            });
        }
        if self.max_file_bytes > HARD_MAX_LINEAR_RECORD_FILE_BYTES {
            return Err(MmapScrollbackError::ReadLimitTooLarge {
                limit_name: "file_bytes",
                maximum: HARD_MAX_LINEAR_RECORD_FILE_BYTES,
                actual: self.max_file_bytes,
            });
        }
        if self.max_records > HARD_MAX_LINEAR_RECORDS {
            return Err(MmapScrollbackError::ReadLimitTooLarge {
                limit_name: "records",
                maximum: u64::try_from(HARD_MAX_LINEAR_RECORDS).unwrap_or(u64::MAX),
                actual: u64::try_from(self.max_records).unwrap_or(u64::MAX),
            });
        }
        if self.max_payload_bytes > HARD_MAX_LINEAR_RECORD_PAYLOAD_BYTES {
            return Err(MmapScrollbackError::ReadLimitTooLarge {
                limit_name: "payload_bytes",
                maximum: HARD_MAX_LINEAR_RECORD_PAYLOAD_BYTES,
                actual: self.max_payload_bytes,
            });
        }
        if self.max_payload_bytes == 0 || self.max_payload_bytes > self.max_file_bytes {
            return Err(MmapScrollbackError::InvalidReadLimit {
                limit_name: "payload_bytes",
            });
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmapScrollbackConfig {
    pub base_dir: PathBuf,
    pub pane_uuid: String,
    pub cap_bytes: u64,
    pub sync_every_appends: u64,
    pub sync_interval: Duration,
}

impl MmapScrollbackConfig {
    #[must_use]
    pub fn new(base_dir: impl Into<PathBuf>, pane_uuid: impl Into<String>) -> Self {
        Self {
            base_dir: base_dir.into(),
            pane_uuid: pane_uuid.into(),
            cap_bytes: DEFAULT_CAP_BYTES,
            sync_every_appends: DEFAULT_SYNC_EVERY_APPENDS,
            sync_interval: DEFAULT_SYNC_INTERVAL,
        }
    }

    #[must_use]
    pub fn with_cap_mb(mut self, cap_mb: u32) -> Self {
        self.cap_bytes = if cap_mb == 0 {
            DEFAULT_CAP_BYTES
        } else {
            u64::from(cap_mb) * 1024 * 1024
        };
        self
    }

    #[must_use]
    pub const fn with_cap_bytes(mut self, cap_bytes: u64) -> Self {
        self.cap_bytes = cap_bytes;
        self
    }

    #[must_use]
    pub const fn with_sync_every_appends(mut self, sync_every_appends: u64) -> Self {
        self.sync_every_appends = sync_every_appends;
        self
    }

    #[must_use]
    pub const fn with_sync_interval(mut self, sync_interval: Duration) -> Self {
        self.sync_interval = sync_interval;
        self
    }

    #[must_use]
    pub fn bin_path(&self) -> PathBuf {
        self.base_dir
            .join(format!("{}.bin", pane_file_stem(&self.pane_uuid)))
    }

    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.base_dir
            .join(format!("{}.bin.lock", pane_file_stem(&self.pane_uuid)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmapAppendReport {
    pub record_kind: RecordKind,
    pub payload_bytes: usize,
    pub redaction: BytesRedactionEvidence,
    pub write_cursor_bytes: u64,
    pub synced: bool,
}

#[derive(Debug)]
pub struct MmapScrollback {
    file: File,
    lock_file: File,
    path: PathBuf,
    lock_path: PathBuf,
    header: ScrollbackHeader,
    appends_since_sync: u64,
    last_sync_at: SystemTime,
    sync_every_appends: u64,
    sync_interval: Duration,
    redactor: StreamingRedactor,
    pending_record_kind: Option<RecordKind>,
    v2_state: V2RingState,
    active_state_slot: usize,
    used_bytes: u64,
    prewrite_sync_required: bool,
    #[cfg(test)]
    sync_observer: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl MmapScrollback {
    pub fn open(config: MmapScrollbackConfig) -> Result<Self, MmapScrollbackError> {
        Self::open_with_after_lock(config, || {})
    }

    fn open_with_after_lock<F>(
        config: MmapScrollbackConfig,
        after_lock: F,
    ) -> Result<Self, MmapScrollbackError>
    where
        F: FnOnce(),
    {
        let cap_bytes = normalize_cap_bytes(config.cap_bytes)?;
        if config
            .base_dir
            .file_name()
            .is_none_or(|leaf| leaf.is_empty())
        {
            return Err(MmapScrollbackError::UnsafeReadSource {
                path: config.base_dir.clone(),
                reason: "writer scrollback directory must name one concrete final leaf",
            });
        }
        let base_directory = ensure_directory_tree_nofollow(&config.base_dir).map_err(|source| {
            MmapScrollbackError::CreateDir {
                path: config.base_dir.clone(),
                source,
            }
        })?;
        secure_writer_directory(&base_directory, &config.base_dir)?;
        let base_directory_identity = metadata_identity(&base_directory.dir_metadata().map_err(
            |source| MmapScrollbackError::Metadata {
                path: config.base_dir.clone(),
                source,
            },
        )?);

        let path = config.bin_path();
        let lock_path = config.lock_path();
        let expected_pane_uuid = pane_uuid_bytes(&config.pane_uuid);
        let lock_leaf = lock_path.file_name().ok_or_else(|| {
            MmapScrollbackError::UnsafeReadSource {
                path: lock_path.clone(),
                reason: "writer lock has no file name",
            }
        })?;
        let opened_lock =
            open_writer_file(&base_directory, Path::new(lock_leaf), &lock_path)?;
        let lock_file = opened_lock.file;
        match lock_file.try_lock_exclusive() {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(MmapScrollbackError::WriterBusy {
                    path: lock_path.clone(),
                });
            }
            Err(source) => {
                return Err(MmapScrollbackError::Lock {
                    path: lock_path.clone(),
                    source,
                });
            }
        }
        after_lock();
        revalidate_writer_file(
            &base_directory,
            Path::new(lock_leaf),
            &lock_path,
            &lock_file,
            opened_lock.identity,
        )?;
        revalidate_writer_directory(
            &base_directory,
            &config.base_dir,
            base_directory_identity,
        )?;

        let data_leaf = path
            .file_name()
            .ok_or_else(|| MmapScrollbackError::UnsafeReadSource {
                path: path.clone(),
                reason: "writer data file has no file name",
            })?;
        let opened_data =
            open_writer_file(&base_directory, Path::new(data_leaf), &path)?;
        let mut file = opened_data.file;
        revalidate_writer_file(
            &base_directory,
            Path::new(data_leaf),
            &path,
            &file,
            opened_data.identity,
        )?;
        revalidate_writer_directory(
            &base_directory,
            &config.base_dir,
            base_directory_identity,
        )?;
        let metadata_len = file
            .metadata()
            .map_err(|source| MmapScrollbackError::Metadata {
                path: path.clone(),
                source,
            })?
            .len();

        let target_len = HEADER_SIZE as u64 + cap_bytes;
        let (header, v2_state, active_state_slot, used_bytes) = if opened_data.created {
            if metadata_len != 0 {
                return Err(MmapScrollbackError::UnsafeReadSource {
                    path: path.clone(),
                    reason: "newly created scrollback leaf was not empty before initialization",
                });
            }
            file.set_len(target_len)
                .map_err(|source| MmapScrollbackError::SetLen {
                    path: path.clone(),
                    len: target_len,
                    source,
                })?;
            let mut header = ScrollbackHeader::new(
                expected_pane_uuid,
                cap_bytes,
                epoch_millis(SystemTime::now()),
            );
            header.flags = HeaderFlags::from_bits(V2_RING_FLAG);
            let state = V2RingState::fresh(cap_bytes)?;
            (header, state, 0, 0)
        } else {
            // Pre-existing evidence is immutable until every structural and
            // authenticated-state invariant has passed. In particular, never
            // `set_len` a short, long, differently configured, or legacy-v1
            // file in place.
            if metadata_len != target_len {
                return Err(MmapScrollbackError::ExistingFileShapeMismatch {
                    path: path.clone(),
                    configured_capacity_bytes: cap_bytes,
                    expected_file_bytes: target_len,
                    actual_file_bytes: metadata_len,
                });
            }
            let mut bytes = [0u8; HEADER_SIZE];
            file.seek(SeekFrom::Start(0))
                .and_then(|_| file.read_exact(&mut bytes))
                .map_err(|source| MmapScrollbackError::ReadHeader {
                    path: path.clone(),
                    source,
                })?;
            let mut decoded = ScrollbackHeader::decode(&bytes)?;
            if decoded.pane_uuid != expected_pane_uuid {
                return Err(MmapScrollbackError::UnsafeReadSource {
                    path: path.clone(),
                    reason: "existing scrollback header does not match its configured filename",
                });
            }
            if decoded.capacity_bytes != cap_bytes {
                return Err(MmapScrollbackError::ExistingFileShapeMismatch {
                    path: path.clone(),
                    configured_capacity_bytes: cap_bytes,
                    expected_file_bytes: target_len,
                    actual_file_bytes: metadata_len,
                });
            }
            if decoded.flags.bits() & V2_RING_FLAG == 0 {
                return Err(MmapScrollbackError::LegacyV1ReadOnly {
                    path: path.clone(),
                });
            }
            let loaded = load_v2_writer_state(&mut file, &path, &bytes, decoded, cap_bytes)?;
            decoded.write_cursor_bytes = u64::from(loaded.state.tail);
            (decoded, loaded.state, loaded.slot_index, loaded.used_bytes)
        };

        let mut writer = Self {
            file,
            lock_file,
            path,
            lock_path,
            header,
            appends_since_sync: 0,
            last_sync_at: SystemTime::now(),
            sync_every_appends: config.sync_every_appends,
            sync_interval: config.sync_interval,
            redactor: StreamingRedactor::new(),
            pending_record_kind: None,
            v2_state,
            active_state_slot,
            used_bytes,
            prewrite_sync_required: false,
            #[cfg(test)]
            sync_observer: None,
        };
        if opened_data.created {
            writer.header.last_msync_at_epoch_ms = epoch_millis(SystemTime::now());
            writer.initialize_v2_file()?;
            writer.sync_file_data()?;
        }
        revalidate_writer_file(
            &base_directory,
            Path::new(data_leaf),
            &path,
            &writer.file,
            opened_data.identity,
        )?;
        revalidate_writer_file(
            &base_directory,
            Path::new(lock_leaf),
            &lock_path,
            &writer.lock_file,
            opened_lock.identity,
        )?;
        revalidate_writer_directory(
            &base_directory,
            &config.base_dir,
            base_directory_identity,
        )?;
        Ok(writer)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    #[must_use]
    pub const fn header(&self) -> ScrollbackHeader {
        self.header
    }

    pub fn append(
        &mut self,
        record_kind: RecordKind,
        payload: &[u8],
    ) -> Result<MmapAppendReport, MmapScrollbackError> {
        let output_record_kind = self.pending_record_kind.unwrap_or(record_kind);
        let redacted = self.redactor.redact_chunk(payload);
        if redacted.bytes.is_empty() && !payload.is_empty() {
            self.pending_record_kind.get_or_insert(record_kind);
            return Ok(MmapAppendReport {
                record_kind,
                payload_bytes: 0,
                redaction: redacted.evidence,
                write_cursor_bytes: self.header.write_cursor_bytes,
                synced: false,
            });
        }

        let report = self.append_redacted_payload(output_record_kind, redacted)?;
        self.pending_record_kind = (self.redactor.pending_bytes() > 0).then_some(record_kind);
        Ok(report)
    }

    /// Flush bytes retained to protect the streaming redaction boundary.
    ///
    /// Call this when a pane stream is shutting down or before intentionally
    /// severing continuity with future appends. Periodic `sync()` deliberately
    /// does not flush this tail: flushing between two adjacent capture chunks
    /// would re-open the split-secret leak this writer is protecting.
    pub fn flush_pending_redaction(
        &mut self,
    ) -> Result<Option<MmapAppendReport>, MmapScrollbackError> {
        if self.redactor.pending_bytes() == 0 {
            self.pending_record_kind = None;
            return Ok(None);
        }

        let record_kind = self.pending_record_kind.unwrap_or(RecordKind::Text);
        let redacted = self.redactor.finish();
        self.pending_record_kind = None;
        if redacted.bytes.is_empty() {
            return Ok(None);
        }

        self.append_redacted_payload(record_kind, redacted)
            .map(Some)
    }

    fn append_redacted_payload(
        &mut self,
        record_kind: RecordKind,
        redacted: RedactionResult,
    ) -> Result<MmapAppendReport, MmapScrollbackError> {
        if self.prewrite_sync_required {
            self.sync()?;
        }
        let payload = fit_payload_to_capacity(redacted.bytes, self.header.capacity_bytes)?;
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| MmapScrollbackError::RecordTooLarge {
                payload_bytes: payload.len(),
                capacity_bytes: self.header.capacity_bytes,
            })?;
        let total_len = V2_RECORD_HEADER_SIZE as u64 + u64::from(payload_len);
        let original_state = self.v2_state;
        let original_used_bytes = self.used_bytes;
        let (mut prepared_state, prepared_used_bytes, evicted) =
            self.plan_append_position(total_len)?;

        // Overwriting a still-referenced record would make both metadata slots
        // unrecoverable if the process died between the body overwrite and the
        // next state publication. Commit and sync the eviction-only state first;
        // this extra boundary occurs only when live bytes must be reclaimed.
        if evicted {
            self.v2_state = prepared_state;
            self.used_bytes = prepared_used_bytes;
            self.header.write_cursor_bytes = u64::from(self.v2_state.tail);
            if let Err(error) = self.publish_v2_state() {
                self.v2_state = original_state;
                self.used_bytes = original_used_bytes;
                self.header.write_cursor_bytes = u64::from(original_state.tail);
                return Err(error);
            }
            self.prewrite_sync_required = true;
            self.sync()?;
            prepared_state = self.v2_state;
        }

        let rollback_state = self.v2_state;
        let rollback_used_bytes = self.used_bytes;
        let start_cursor = u64::from(prepared_state.tail);
        let record = encode_v2_record_header(
            self.header.pane_uuid,
            record_kind,
            payload_len,
            prepared_state.generation,
            prepared_state.next_sequence,
            &payload,
        );
        let offset = HEADER_SIZE as u64 + start_cursor;

        if let Err(source) = self
            .file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.write_all(&record))
            .and_then(|()| self.file.write_all(&payload))
        {
            if !evicted {
                self.v2_state = original_state;
                self.used_bytes = original_used_bytes;
            } else {
                self.v2_state = rollback_state;
                self.used_bytes = rollback_used_bytes;
            }
            return Err(MmapScrollbackError::WriteRecord {
                path: self.path.clone(),
                source,
            });
        }

        let mut committed_state = prepared_state;
        if committed_state.record_count == 0 {
            committed_state.head = committed_state.tail;
        }
        committed_state.record_count = committed_state
            .record_count
            .checked_add(1)
            .ok_or_else(|| MmapScrollbackError::V2Integrity {
                path: self.path.clone(),
                reason: "v2 record count overflow",
            })?;
        committed_state.next_sequence = committed_state
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| MmapScrollbackError::V2Integrity {
                path: self.path.clone(),
                reason: "v2 record sequence exhausted",
            })?;
        let end_cursor = start_cursor + total_len;
        if end_cursor == self.header.capacity_bytes {
            committed_state.tail = 0;
            committed_state.wrap_at = u32::try_from(self.header.capacity_bytes).map_err(|_| {
                MmapScrollbackError::V2Integrity {
                    path: self.path.clone(),
                    reason: "v2 capacity does not fit ring offsets",
                }
            })?;
            committed_state.generation = committed_state.generation.checked_add(1).ok_or_else(
                || MmapScrollbackError::V2Integrity {
                    path: self.path.clone(),
                    reason: "v2 ring generation exhausted",
                },
            )?;
        } else {
            committed_state.tail = u32::try_from(end_cursor).map_err(|_| {
                MmapScrollbackError::V2Integrity {
                    path: self.path.clone(),
                    reason: "v2 tail does not fit ring offset",
                }
            })?;
        }

        let committed_used_bytes = prepared_used_bytes.checked_add(total_len).ok_or_else(|| {
            MmapScrollbackError::V2Integrity {
                path: self.path.clone(),
                reason: "v2 used-byte accounting overflow",
            }
        })?;
        if committed_used_bytes > self.header.capacity_bytes {
            return Err(MmapScrollbackError::V2Integrity {
                path: self.path.clone(),
                reason: "v2 append plan exceeds ring capacity",
            });
        }
        self.v2_state = committed_state;
        self.used_bytes = committed_used_bytes;
        self.header.write_cursor_bytes = u64::from(self.v2_state.tail);
        self.header.total_bytes_written = self.header.total_bytes_written.saturating_add(total_len);
        self.header.redactions_applied = self
            .header
            .redactions_applied
            .saturating_add(u64::from(redacted.evidence.replacement_count));
        if let Err(error) = self.publish_v2_state() {
            self.v2_state = rollback_state;
            self.used_bytes = rollback_used_bytes;
            self.header.write_cursor_bytes = u64::from(self.v2_state.tail);
            self.header.total_bytes_written =
                self.header.total_bytes_written.saturating_sub(total_len);
            self.header.redactions_applied = self
                .header
                .redactions_applied
                .saturating_sub(u64::from(redacted.evidence.replacement_count));
            return Err(error);
        }

        self.appends_since_sync = self.appends_since_sync.saturating_add(1);
        let synced = self.sync_if_due()?;

        Ok(MmapAppendReport {
            record_kind,
            payload_bytes: payload.len(),
            redaction: redacted.evidence,
            write_cursor_bytes: self.header.write_cursor_bytes,
            synced,
        })
    }

    pub fn sync(&mut self) -> Result<(), MmapScrollbackError> {
        self.sync_current_state()?;
        self.prewrite_sync_required = false;
        Ok(())
    }

    fn sync_current_state(&mut self) -> Result<(), MmapScrollbackError> {
        self.header.last_msync_at_epoch_ms = epoch_millis(SystemTime::now());
        self.write_v2_telemetry()?;
        self.sync_file_data()?;
        self.last_sync_at = SystemTime::now();
        self.appends_since_sync = 0;
        Ok(())
    }

    fn sync_if_due(&mut self) -> Result<bool, MmapScrollbackError> {
        let append_due =
            self.sync_every_appends > 0 && self.appends_since_sync >= self.sync_every_appends;
        let time_due = self
            .last_sync_at
            .elapsed()
            .is_ok_and(|elapsed| elapsed >= self.sync_interval);
        if append_due || time_due {
            self.sync()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn initialize_v2_file(&mut self) -> Result<(), MmapScrollbackError> {
        let mut bytes = encode_v2_base_header(self.header);
        let slot = encode_v2_state_slot(&bytes, self.v2_state);
        bytes[V2_STATE_SLOTS_OFFSET..V2_STATE_SLOTS_OFFSET + V2_STATE_SLOT_SIZE]
            .copy_from_slice(&slot);
        self.file
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.file.write_all(&bytes))
            .map_err(|source| MmapScrollbackError::WriteHeader {
                path: self.path.clone(),
                source,
            })
    }

    fn write_v2_telemetry(&mut self) -> Result<(), MmapScrollbackError> {
        let bytes = encode_v2_base_header(self.header);
        self.file
            .seek(SeekFrom::Start(64))
            .and_then(|_| self.file.write_all(&bytes[64..88]))
            .map_err(|source| MmapScrollbackError::WriteHeader {
                path: self.path.clone(),
                source,
            })
    }

    fn publish_v2_state(&mut self) -> Result<(), MmapScrollbackError> {
        let mut next = self.v2_state;
        next.slot_epoch = next.slot_epoch.checked_add(1).ok_or_else(|| {
            MmapScrollbackError::V2Integrity {
                path: self.path.clone(),
                reason: "v2 state epoch exhausted",
            }
        })?;
        let next_slot = (self.active_state_slot + 1) % V2_STATE_SLOT_COUNT;
        let base = encode_v2_base_header(self.header);
        let encoded = encode_v2_state_slot(&base, next);
        let offset = V2_STATE_SLOTS_OFFSET + next_slot * V2_STATE_SLOT_SIZE;
        self.file
            .seek(SeekFrom::Start(offset as u64))
            .and_then(|_| self.file.write_all(&encoded))
            .map_err(|source| MmapScrollbackError::WriteHeader {
                path: self.path.clone(),
                source,
            })?;
        self.v2_state = next;
        self.active_state_slot = next_slot;
        Ok(())
    }

    fn sync_file_data(&self) -> Result<(), MmapScrollbackError> {
        #[cfg(test)]
        if let Some(observer) = &self.sync_observer {
            observer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.file
            .sync_data()
            .map_err(|source| MmapScrollbackError::Sync {
                path: self.path.clone(),
                source,
            })
    }

    fn plan_append_position(
        &mut self,
        total_len: u64,
    ) -> Result<(V2RingState, u64, bool), MmapScrollbackError> {
        let capacity = self.header.capacity_bytes;
        let capacity_u32 = u32::try_from(capacity).map_err(|_| {
            MmapScrollbackError::V2Integrity {
                path: self.path.clone(),
                reason: "v2 capacity does not fit ring offsets",
            }
        })?;
        let mut state = self.v2_state;
        let mut used_bytes = self.used_bytes;
        let mut evicted = false;

        if state.record_count == 0 {
            state.head = 0;
            state.tail = 0;
            state.wrap_at = capacity_u32;
            used_bytes = 0;
            return Ok((state, used_bytes, false));
        }

        while usize::try_from(state.record_count).unwrap_or(usize::MAX)
            >= HARD_MAX_LINEAR_RECORDS
        {
            self.evict_oldest_from_plan(&mut state, &mut used_bytes)?;
            evicted = true;
        }

        loop {
            let tail = u64::from(state.tail);
            let head = u64::from(state.head);
            if tail < head {
                if tail + total_len <= head {
                    break;
                }
                self.evict_oldest_from_plan(&mut state, &mut used_bytes)?;
                evicted = true;
                continue;
            }

            let ring_full = state.record_count > 0 && state.tail == state.head;
            if !ring_full && tail + total_len <= capacity {
                break;
            }

            if tail + total_len > capacity {
                while state.record_count > 0 && total_len > u64::from(state.head) {
                    self.evict_oldest_from_plan(&mut state, &mut used_bytes)?;
                    evicted = true;
                }
                if state.record_count == 0 {
                    state.head = 0;
                    state.tail = 0;
                    state.wrap_at = capacity_u32;
                    used_bytes = 0;
                } else {
                    state.wrap_at = state.tail;
                    state.tail = 0;
                }
                state.generation = state.generation.checked_add(1).ok_or_else(|| {
                    MmapScrollbackError::V2Integrity {
                        path: self.path.clone(),
                        reason: "v2 ring generation exhausted",
                    }
                })?;
                break;
            }

            self.evict_oldest_from_plan(&mut state, &mut used_bytes)?;
            evicted = true;
        }

        Ok((state, used_bytes, evicted))
    }

    fn evict_oldest_from_plan(
        &mut self,
        state: &mut V2RingState,
        used_bytes: &mut u64,
    ) -> Result<(), MmapScrollbackError> {
        if state.record_count == 0 {
            return Err(MmapScrollbackError::V2Integrity {
                path: self.path.clone(),
                reason: "attempted to evict from an empty v2 ring",
            });
        }
        if state.head == state.wrap_at && state.head != 0 {
            state.head = 0;
            state.wrap_at = u32::try_from(self.header.capacity_bytes).map_err(|_| {
                MmapScrollbackError::V2Integrity {
                    path: self.path.clone(),
                    reason: "v2 capacity does not fit ring offsets",
                }
            })?;
        }
        let meta = read_v2_record_meta_verified(
            &mut self.file,
            &self.path,
            self.header,
            u64::from(state.head),
            self.header.capacity_bytes,
        )?;
        let record_end = u64::from(state.head) + u64::from(meta.total_len);
        *used_bytes = used_bytes.checked_sub(u64::from(meta.total_len)).ok_or_else(|| {
            MmapScrollbackError::V2Integrity {
                path: self.path.clone(),
                reason: "v2 used-byte accounting underflow",
            }
        })?;
        state.record_count -= 1;
        if state.record_count == 0 {
            state.head = state.tail;
            state.wrap_at = u32::try_from(self.header.capacity_bytes).map_err(|_| {
                MmapScrollbackError::V2Integrity {
                    path: self.path.clone(),
                    reason: "v2 capacity does not fit ring offsets",
                }
            })?;
            return Ok(());
        }
        if record_end == u64::from(state.wrap_at) {
            state.head = 0;
            state.wrap_at = u32::try_from(self.header.capacity_bytes).map_err(|_| {
                MmapScrollbackError::V2Integrity {
                    path: self.path.clone(),
                    reason: "v2 capacity does not fit ring offsets",
                }
            })?;
        } else if record_end == self.header.capacity_bytes {
            state.head = 0;
        } else {
            state.head = u32::try_from(record_end).map_err(|_| {
                MmapScrollbackError::V2Integrity {
                    path: self.path.clone(),
                    reason: "v2 head does not fit ring offset",
                }
            })?;
        }
        Ok(())
    }
}

impl Drop for MmapScrollback {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
}

#[derive(Debug, Clone, Copy)]
struct LoadedV2State {
    state: V2RingState,
    slot_index: usize,
    used_bytes: u64,
}

#[derive(Debug)]
struct DecodedV2Records {
    records: Vec<(RecordKind, Vec<u8>)>,
    payload_bytes: u64,
    used_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
enum V2SlotObservation {
    Empty,
    Valid(V2RingState),
    Invalid,
}

fn encode_v2_base_header(mut header: ScrollbackHeader) -> [u8; HEADER_SIZE] {
    // v2 replay authority lives exclusively in the checksummed dual slots.
    // Keep the v1 cursor zero on disk so a reader that ignores the v2 flag can
    // never mis-present a wrapped physical prefix as an ordered export.
    header.write_cursor_bytes = 0;
    header.encode()
}

fn encode_v2_state_slot(
    base_header: &[u8; HEADER_SIZE],
    state: V2RingState,
) -> [u8; V2_STATE_SLOT_SIZE] {
    let mut slot = [0u8; V2_STATE_SLOT_SIZE];
    slot[0..4].copy_from_slice(&V2_STATE_SLOT_MAGIC);
    slot[4..6].copy_from_slice(&V2_STATE_SLOT_VERSION.to_le_bytes());
    slot[6..8].copy_from_slice(&0u16.to_le_bytes());
    slot[8..16].copy_from_slice(&state.slot_epoch.to_le_bytes());
    slot[16..20].copy_from_slice(&state.head.to_le_bytes());
    slot[20..24].copy_from_slice(&state.tail.to_le_bytes());
    slot[24..28].copy_from_slice(&state.wrap_at.to_le_bytes());
    slot[28..32].copy_from_slice(&state.record_count.to_le_bytes());
    slot[32..40].copy_from_slice(&state.generation.to_le_bytes());
    slot[40..48].copy_from_slice(&state.next_sequence.to_le_bytes());
    let checksum = v2_state_checksum(base_header, &slot[..V2_STATE_CHECKSUM_OFFSET]);
    slot[V2_STATE_CHECKSUM_OFFSET
        ..V2_STATE_CHECKSUM_OFFSET + V2_STATE_CHECKSUM_BYTES]
        .copy_from_slice(&checksum);
    slot
}

fn v2_state_checksum(
    base_header: &[u8; HEADER_SIZE],
    state_prefix: &[u8],
) -> [u8; V2_STATE_CHECKSUM_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(V2_STATE_HASH_DOMAIN);
    // Capacity, pane identity, creation time, outer version, and flags are
    // immutable for the lifetime of a v2 leaf. Mutable telemetry starts at 64.
    hasher.update(&base_header[..64]);
    hasher.update(state_prefix);
    let digest = hasher.finalize();
    let mut checksum = [0u8; V2_STATE_CHECKSUM_BYTES];
    checksum.copy_from_slice(&digest[..V2_STATE_CHECKSUM_BYTES]);
    checksum
}

fn observe_v2_state_slot(
    base_header: &[u8; HEADER_SIZE],
    slot_index: usize,
) -> V2SlotObservation {
    let offset = V2_STATE_SLOTS_OFFSET + slot_index * V2_STATE_SLOT_SIZE;
    let slot = &base_header[offset..offset + V2_STATE_SLOT_SIZE];
    if slot.iter().all(|byte| *byte == 0) {
        return V2SlotObservation::Empty;
    }
    if slot[..4] != V2_STATE_SLOT_MAGIC
        || u16::from_le_bytes([slot[4], slot[5]]) != V2_STATE_SLOT_VERSION
        || u16::from_le_bytes([slot[6], slot[7]]) != 0
    {
        return V2SlotObservation::Invalid;
    }
    let expected = v2_state_checksum(base_header, &slot[..V2_STATE_CHECKSUM_OFFSET]);
    if slot[V2_STATE_CHECKSUM_OFFSET
        ..V2_STATE_CHECKSUM_OFFSET + V2_STATE_CHECKSUM_BYTES]
        != expected
    {
        return V2SlotObservation::Invalid;
    }
    V2SlotObservation::Valid(V2RingState {
        slot_epoch: u64::from_le_bytes(slot[8..16].try_into().unwrap()),
        head: u32::from_le_bytes(slot[16..20].try_into().unwrap()),
        tail: u32::from_le_bytes(slot[20..24].try_into().unwrap()),
        wrap_at: u32::from_le_bytes(slot[24..28].try_into().unwrap()),
        record_count: u32::from_le_bytes(slot[28..32].try_into().unwrap()),
        generation: u64::from_le_bytes(slot[32..40].try_into().unwrap()),
        next_sequence: u64::from_le_bytes(slot[40..48].try_into().unwrap()),
    })
}

fn v2_slot_candidates(
    header_bytes: &[u8; HEADER_SIZE],
) -> (Vec<(V2RingState, usize)>, bool) {
    let mut candidates = Vec::with_capacity(V2_STATE_SLOT_COUNT);
    let mut saw_invalid = false;
    for slot_index in 0..V2_STATE_SLOT_COUNT {
        match observe_v2_state_slot(header_bytes, slot_index) {
            V2SlotObservation::Empty => {}
            V2SlotObservation::Valid(state) => candidates.push((state, slot_index)),
            V2SlotObservation::Invalid => saw_invalid = true,
        }
    }
    candidates.sort_by(|left, right| right.0.slot_epoch.cmp(&left.0.slot_epoch));
    (candidates, saw_invalid)
}

fn encode_v2_record_header(
    pane_uuid: [u8; 32],
    record_kind: RecordKind,
    payload_len: u32,
    generation: u64,
    sequence: u64,
    payload: &[u8],
) -> [u8; V2_RECORD_HEADER_SIZE] {
    let mut header = [0u8; V2_RECORD_HEADER_SIZE];
    header[0..4].copy_from_slice(&V2_RECORD_MAGIC);
    header[4..8].copy_from_slice(&payload_len.to_le_bytes());
    header[8] = record_kind.as_u8();
    header[16..24].copy_from_slice(&generation.to_le_bytes());
    header[24..32].copy_from_slice(&sequence.to_le_bytes());
    let digest = v2_record_digest(
        pane_uuid,
        record_kind,
        payload_len,
        generation,
        sequence,
        payload,
    );
    header[V2_RECORD_DIGEST_OFFSET..V2_RECORD_DIGEST_OFFSET + V2_RECORD_DIGEST_BYTES]
        .copy_from_slice(&digest);
    header
}

fn v2_record_digest(
    pane_uuid: [u8; 32],
    record_kind: RecordKind,
    payload_len: u32,
    generation: u64,
    sequence: u64,
    payload: &[u8],
) -> [u8; V2_RECORD_DIGEST_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(V2_RECORD_HASH_DOMAIN);
    hasher.update(pane_uuid);
    hasher.update([record_kind.as_u8()]);
    hasher.update(payload_len.to_le_bytes());
    hasher.update(generation.to_le_bytes());
    hasher.update(sequence.to_le_bytes());
    hasher.update(payload);
    hasher.finalize().into()
}

fn read_v2_record_verified(
    file: &mut File,
    path: &Path,
    header: ScrollbackHeader,
    body_offset: u64,
    capacity_bytes: u64,
    max_payload_bytes: u64,
) -> Result<(RecordKind, Vec<u8>, V2RecordMeta), MmapScrollbackError> {
    if body_offset
        .checked_add(V2_RECORD_HEADER_SIZE as u64)
        .is_none_or(|end| end > capacity_bytes)
    {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 record header crosses the ring boundary",
        });
    }
    let mut bytes = [0u8; V2_RECORD_HEADER_SIZE];
    file.seek(SeekFrom::Start(HEADER_SIZE as u64 + body_offset))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|source| MmapScrollbackError::ReadRecord {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes[..4] != V2_RECORD_MAGIC || bytes[9..16].iter().any(|byte| *byte != 0) {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 record marker or reserved bytes are invalid",
        });
    }
    let payload_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if u64::from(payload_len) > max_payload_bytes {
        return Err(MmapScrollbackError::ReadLimitExceeded {
            path: path.to_path_buf(),
            limit_name: "payload_bytes",
            limit: max_payload_bytes,
            observed: u64::from(payload_len),
        });
    }
    let total_len = (V2_RECORD_HEADER_SIZE as u64)
        .checked_add(u64::from(payload_len))
        .ok_or_else(|| MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 record length overflow",
        })?;
    if body_offset
        .checked_add(total_len)
        .is_none_or(|end| end > capacity_bytes)
    {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 record payload crosses the ring boundary",
        });
    }
    let record_kind = RecordKind::from_u8(bytes[8]).ok_or_else(|| {
        MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 record kind is unknown",
        }
    })?;
    let generation = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let sequence = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    let mut payload = vec![0u8; payload_len as usize];
    file.read_exact(&mut payload)
        .map_err(|source| MmapScrollbackError::ReadRecord {
            path: path.to_path_buf(),
            source,
        })?;
    let expected = v2_record_digest(
        header.pane_uuid,
        record_kind,
        payload_len,
        generation,
        sequence,
        &payload,
    );
    if bytes[V2_RECORD_DIGEST_OFFSET..V2_RECORD_DIGEST_OFFSET + V2_RECORD_DIGEST_BYTES]
        != expected
    {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 record digest mismatch",
        });
    }
    Ok((
        record_kind,
        payload,
        V2RecordMeta {
            total_len: u32::try_from(total_len).map_err(|_| {
                MmapScrollbackError::V2Integrity {
                    path: path.to_path_buf(),
                    reason: "v2 record length does not fit its format",
                }
            })?,
            payload_len,
            generation,
            sequence,
        },
    ))
}

fn read_v2_record_meta_verified(
    file: &mut File,
    path: &Path,
    header: ScrollbackHeader,
    body_offset: u64,
    capacity_bytes: u64,
) -> Result<V2RecordMeta, MmapScrollbackError> {
    read_v2_record_verified(
        file,
        path,
        header,
        body_offset,
        capacity_bytes,
        capacity_bytes,
    )
    .map(|(_, _, meta)| meta)
}

fn validate_v2_state_shape(
    path: &Path,
    state: V2RingState,
    capacity_bytes: u64,
) -> Result<(), MmapScrollbackError> {
    let capacity = u32::try_from(capacity_bytes).map_err(|_| {
        MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 capacity does not fit ring offsets",
        }
    })?;
    if state.head >= capacity || state.tail >= capacity || state.wrap_at > capacity {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 state offset lies outside the ring",
        });
    }
    if state.record_count == 0 {
        if state.head != state.tail || state.wrap_at != capacity {
            return Err(MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "empty v2 state has inconsistent head, tail, or wrap boundary",
            });
        }
        return Ok(());
    }
    if u64::from(state.record_count) > state.next_sequence {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 retained count exceeds the next sequence",
        });
    }
    if state.tail < state.head {
        if state.wrap_at < state.head || state.wrap_at > capacity {
            return Err(MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "wrapped v2 state has an invalid wrap boundary",
            });
        }
    } else if state.tail > state.head && state.wrap_at != capacity {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "linear v2 state unexpectedly retains a wrap boundary",
        });
    } else if state.tail == state.head && state.wrap_at < state.head {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "full v2 state has an invalid wrap boundary",
        });
    }
    Ok(())
}

fn decode_v2_records_for_state(
    file: &mut File,
    path: &Path,
    header: ScrollbackHeader,
    state: V2RingState,
    limits: LinearRecordReadLimits,
    collect_records: bool,
) -> Result<DecodedV2Records, MmapScrollbackError> {
    validate_v2_state_shape(path, state, header.capacity_bytes)?;
    if state.record_count as usize > limits.max_records {
        return Err(MmapScrollbackError::ReadLimitExceeded {
            path: path.to_path_buf(),
            limit_name: "records",
            limit: limits.max_records as u64,
            observed: u64::from(state.record_count),
        });
    }
    let mut cursor = u64::from(state.head);
    let mut records = if collect_records {
        Vec::with_capacity(state.record_count as usize)
    } else {
        Vec::new()
    };
    let mut payload_bytes = 0u64;
    let mut used_bytes = 0u64;
    let mut wrapped = false;
    let first_sequence = state
        .next_sequence
        .checked_sub(u64::from(state.record_count))
        .ok_or_else(|| MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 sequence/count accounting underflow",
        })?;
    let mut previous_generation = None;

    for record_index in 0..state.record_count {
        if cursor == u64::from(state.wrap_at) && cursor != 0 {
            if wrapped {
                return Err(MmapScrollbackError::V2Integrity {
                    path: path.to_path_buf(),
                    reason: "v2 retained interval wraps more than once",
                });
            }
            cursor = 0;
            wrapped = true;
        }
        let remaining_payload = limits.max_payload_bytes.saturating_sub(payload_bytes);
        let (kind, payload, meta) = read_v2_record_verified(
            file,
            path,
            header,
            cursor,
            header.capacity_bytes,
            remaining_payload,
        )?;
        let expected_sequence = first_sequence + u64::from(record_index);
        if meta.sequence != expected_sequence || meta.generation > state.generation {
            return Err(MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "v2 record sequence or generation is inconsistent with state",
            });
        }
        if previous_generation.is_some_and(|previous| meta.generation < previous) {
            return Err(MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "v2 record generations are not monotone",
            });
        }
        previous_generation = Some(meta.generation);
        payload_bytes = payload_bytes
            .checked_add(u64::from(meta.payload_len))
            .ok_or_else(|| MmapScrollbackError::ReadLimitExceeded {
                path: path.to_path_buf(),
                limit_name: "payload_bytes",
                limit: limits.max_payload_bytes,
                observed: u64::MAX,
            })?;
        used_bytes = used_bytes
            .checked_add(u64::from(meta.total_len))
            .ok_or_else(|| MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "v2 used-byte accounting overflow",
            })?;
        if used_bytes > header.capacity_bytes {
            return Err(MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "v2 retained records exceed ring capacity",
            });
        }
        if collect_records {
            records.push((kind, payload));
        }
        cursor += u64::from(meta.total_len);
        if cursor == u64::from(state.wrap_at) || cursor == header.capacity_bytes {
            if cursor == u64::from(state.wrap_at) && state.wrap_at != 0
                || cursor == header.capacity_bytes
            {
                if record_index + 1 < state.record_count && wrapped {
                    return Err(MmapScrollbackError::V2Integrity {
                        path: path.to_path_buf(),
                        reason: "v2 retained interval wraps more than once",
                    });
                }
                cursor = 0;
                wrapped = true;
            }
        }
    }
    if cursor != u64::from(state.tail) {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 retained records do not terminate at the published tail",
        });
    }
    Ok(DecodedV2Records {
        records,
        payload_bytes,
        used_bytes,
    })
}

fn load_v2_writer_state(
    file: &mut File,
    path: &Path,
    header_bytes: &[u8; HEADER_SIZE],
    header: ScrollbackHeader,
    capacity_bytes: u64,
) -> Result<LoadedV2State, MmapScrollbackError> {
    if header.write_cursor_bytes != 0 || header.flags.bits() != V2_RING_FLAG {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 outer header fields are not canonical",
        });
    }
    let (candidates, saw_invalid) = v2_slot_candidates(header_bytes);
    if saw_invalid || candidates.is_empty() {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 state slot is missing, torn, or corrupt",
        });
    }
    if candidates.len() == 2 && candidates[0].0.slot_epoch == candidates[1].0.slot_epoch {
        return Err(MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "v2 state slots have an ambiguous epoch",
        });
    }
    let (state, slot_index) = candidates[0];
    let decoded = decode_v2_records_for_state(
        file,
        path,
        header,
        state,
        LinearRecordReadLimits {
            max_file_bytes: HEADER_SIZE as u64 + capacity_bytes,
            max_records: HARD_MAX_LINEAR_RECORDS,
            max_payload_bytes: HARD_MAX_LINEAR_RECORD_PAYLOAD_BYTES,
        },
        false,
    )?;
    Ok(LoadedV2State {
        state,
        slot_index,
        used_bytes: decoded.used_bytes,
    })
}

/// Decode the linear committed prefix of a legacy mmap scrollback file.
///
/// The caller must provide finite limits. Every caller-controlled path
/// component is opened without following symlinks, the leaf must be one
/// owner-private regular file with one link, and the path/descriptor identity
/// is revalidated after the read.
pub fn read_linear_records(
    path: &Path,
    limits: LinearRecordReadLimits,
) -> Result<LinearRecordSnapshot, MmapScrollbackError> {
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or_else(|| MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "missing file name",
        })?;
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = open_directory_tree_nofollow(parent_path).map_err(|source| {
        MmapScrollbackError::Open {
            path: parent_path.to_path_buf(),
            source,
        }
    })?;
    let parent_metadata = parent.dir_metadata().map_err(|source| {
        MmapScrollbackError::Metadata {
            path: parent_path.to_path_buf(),
            source,
        }
    })?;
    validate_private_cap_directory(&parent_metadata, parent_path)?;
    let parent_identity = metadata_identity(&parent_metadata);
    let records = read_linear_records_in_directory(&parent, Path::new(leaf), path, limits)?;
    let reopened = open_directory_tree_nofollow(parent_path).map_err(|source| {
        MmapScrollbackError::Open {
            path: parent_path.to_path_buf(),
            source,
        }
    })?;
    let reopened_metadata = reopened.dir_metadata().map_err(|source| {
        MmapScrollbackError::Metadata {
            path: parent_path.to_path_buf(),
            source,
        }
    })?;
    validate_private_cap_directory(&reopened_metadata, parent_path)?;
    let reopened_identity = metadata_identity(&reopened_metadata);
    if parent_identity != reopened_identity {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: parent_path.to_path_buf(),
            reason: "parent directory identity changed during read",
        });
    }
    Ok(records)
}

pub(crate) fn read_linear_records_in_directory(
    directory: &CapDir,
    leaf: &Path,
    display_path: &Path,
    limits: LinearRecordReadLimits,
) -> Result<LinearRecordSnapshot, MmapScrollbackError> {
    read_linear_records_in_directory_with_hook(directory, leaf, display_path, limits, || {})
}

fn read_linear_records_in_directory_with_hook<F>(
    directory: &CapDir,
    leaf: &Path,
    display_path: &Path,
    limits: LinearRecordReadLimits,
    after_open: F,
) -> Result<LinearRecordSnapshot, MmapScrollbackError>
where
    F: FnOnce(),
{
    let limits = limits.validate()?;
    if leaf.components().count() != 1 {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: display_path.to_path_buf(),
            reason: "scrollback leaf is not one path component",
        });
    }
    let directory_metadata = directory.dir_metadata().map_err(|source| {
        MmapScrollbackError::Metadata {
            path: display_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            source,
        }
    })?;
    validate_private_cap_directory(
        &directory_metadata,
        display_path.parent().unwrap_or_else(|| Path::new(".")),
    )?;
    let path_metadata_before = directory.symlink_metadata(leaf).map_err(|source| {
        MmapScrollbackError::Metadata {
            path: display_path.to_path_buf(),
            source,
        }
    })?;
    validate_private_cap_file(&path_metadata_before, display_path)?;
    validate_owner_against_directory(&path_metadata_before, &directory_metadata, display_path)?;
    if path_metadata_before.len() > limits.max_file_bytes {
        return Err(MmapScrollbackError::ReadLimitExceeded {
            path: display_path.to_path_buf(),
            limit_name: "file_bytes",
            limit: limits.max_file_bytes,
            observed: path_metadata_before.len(),
        });
    }

    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = directory
        .open_with(leaf, &options)
        .map_err(|source| MmapScrollbackError::Open {
            path: display_path.to_path_buf(),
            source,
        })?
        .into_std();
    let handle_metadata_before = file.metadata().map_err(|source| {
        MmapScrollbackError::Metadata {
            path: display_path.to_path_buf(),
            source,
        }
    })?;
    validate_private_std_file(&handle_metadata_before, display_path)?;
    validate_same_owner(&path_metadata_before, &handle_metadata_before, display_path)?;
    if metadata_identity(&path_metadata_before) != metadata_identity(&handle_metadata_before) {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: display_path.to_path_buf(),
            reason: "path and opened descriptor identities differ",
        });
    }
    if handle_metadata_before.len() > limits.max_file_bytes {
        return Err(MmapScrollbackError::ReadLimitExceeded {
            path: display_path.to_path_buf(),
            limit_name: "file_bytes",
            limit: limits.max_file_bytes,
            observed: handle_metadata_before.len(),
        });
    }

    after_open();
    let records = read_linear_records_from_file(
        &mut file,
        display_path,
        handle_metadata_before.len(),
        metadata_identity(&handle_metadata_before),
        limits,
    )?;

    let handle_metadata_after = file.metadata().map_err(|source| {
        MmapScrollbackError::Metadata {
            path: display_path.to_path_buf(),
            source,
        }
    })?;
    let path_metadata_after = directory.symlink_metadata(leaf).map_err(|source| {
        MmapScrollbackError::Metadata {
            path: display_path.to_path_buf(),
            source,
        }
    })?;
    validate_private_std_file(&handle_metadata_after, display_path)?;
    validate_private_cap_file(&path_metadata_after, display_path)?;
    validate_owner_against_directory(&path_metadata_after, &directory_metadata, display_path)?;
    validate_same_owner(&path_metadata_after, &handle_metadata_after, display_path)?;
    let initial_identity = metadata_identity(&handle_metadata_before);
    if metadata_identity(&path_metadata_after) != initial_identity
        || metadata_identity(&handle_metadata_after) != initial_identity
        || handle_metadata_after.len() != handle_metadata_before.len()
    {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: display_path.to_path_buf(),
            reason: "scrollback file changed identity or size during read",
        });
    }
    Ok(records)
}

fn read_linear_records_from_file(
    file: &mut File,
    path: &Path,
    file_len: u64,
    source_identity: LinearRecordSourceIdentity,
    limits: LinearRecordReadLimits,
) -> Result<LinearRecordSnapshot, MmapScrollbackError> {
    let mut header_bytes = [0u8; HEADER_SIZE];
    file.seek(SeekFrom::Start(0))
        .map_err(|source| MmapScrollbackError::ReadHeader {
            path: path.to_path_buf(),
            source,
        })?;
    file.read_exact(&mut header_bytes)
        .map_err(|source| MmapScrollbackError::ReadHeader {
            path: path.to_path_buf(),
            source,
        })?;
    let header = ScrollbackHeader::decode(&header_bytes)?;
    if header.flags.bits() & V2_RING_FLAG != 0 {
        if header.flags.bits() != V2_RING_FLAG || header.write_cursor_bytes != 0 {
            return Err(MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "v2 outer header fields are not canonical",
            });
        }
        let expected_file_len = (HEADER_SIZE as u64)
            .checked_add(header.capacity_bytes)
            .ok_or_else(|| MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "v2 physical length overflow",
            })?;
        if file_len != expected_file_len {
            return Err(MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "v2 physical length does not match its declared capacity",
            });
        }
        let (candidates, saw_invalid_slot) = v2_slot_candidates(&header_bytes);
        if candidates.is_empty() {
            return Err(MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "v2 has no authenticated state slot",
            });
        }
        if candidates.len() == 2 && candidates[0].0.slot_epoch == candidates[1].0.slot_epoch {
            return Err(MmapScrollbackError::V2Integrity {
                path: path.to_path_buf(),
                reason: "v2 state slots have an ambiguous epoch",
            });
        }

        let mut rejected_newer_state = saw_invalid_slot;
        let mut rejected_tail = None;
        let mut last_error = None;
        for (state, _) in candidates {
            match decode_v2_records_for_state(file, path, header, state, limits, true) {
                Ok(decoded) => {
                    let mut decoded_header = header;
                    decoded_header.write_cursor_bytes = u64::from(state.tail);
                    let completeness = if rejected_newer_state {
                        LinearRecordCompleteness::Incomplete {
                            decoded_cursor_bytes: u64::from(state.tail),
                            declared_cursor_bytes: rejected_tail
                                .unwrap_or(header.capacity_bytes),
                            reason: LinearRecordTerminalReason::V2NewerStateInvalid,
                        }
                    } else {
                        LinearRecordCompleteness::Complete
                    };
                    return Ok(LinearRecordSnapshot {
                        header: decoded_header,
                        records: decoded.records,
                        payload_bytes: decoded.payload_bytes,
                        source_identity,
                        completeness,
                    });
                }
                Err(error @ MmapScrollbackError::ReadLimitExceeded { .. }) => {
                    return Err(error);
                }
                Err(error) => {
                    rejected_newer_state = true;
                    rejected_tail = Some(u64::from(state.tail));
                    last_error = Some(error);
                }
            }
        }
        return Err(last_error.unwrap_or_else(|| MmapScrollbackError::V2Integrity {
            path: path.to_path_buf(),
            reason: "no v2 state slot references a valid retained interval",
        }));
    }
    // The header's write_cursor/capacity are themselves on-disk values that may
    // be corrupt or stale — this recovery path exists precisely because a crash
    // can leave a header whose cursor was bumped before the payload flushed. So
    // bound every payload allocation by the *actual* file length, never by the
    // header alone: without this, `record_len` (an on-disk u32) drives
    // `vec![0u8; record_len]` and a corrupt small file claiming a multi-GB
    // record would reserve ~4 GB before `read_exact` fails.
    let mut cursor = 0u64;
    let mut records = Vec::new();
    let mut payload_bytes = 0u64;
    let mut terminal_reason = None;

    while cursor < header.write_cursor_bytes {
        let Some(declared_header_end) = cursor.checked_add(RECORD_HEADER_SIZE as u64) else {
            return Err(MmapScrollbackError::UnsafeReadSource {
                path: path.to_path_buf(),
                reason: "record header offset overflow",
            });
        };
        if declared_header_end > header.write_cursor_bytes {
            terminal_reason = Some(LinearRecordTerminalReason::DeclaredTailTooShortForHeader);
            break;
        }
        if (HEADER_SIZE as u64)
            .checked_add(cursor)
            .and_then(|offset| offset.checked_add(RECORD_HEADER_SIZE as u64))
            .is_none_or(|record_header_end| record_header_end > file_len)
        {
            terminal_reason = Some(LinearRecordTerminalReason::PhysicalRecordHeaderTruncated);
            break;
        }
        if records.len() >= limits.max_records {
            return Err(MmapScrollbackError::ReadLimitExceeded {
                path: path.to_path_buf(),
                limit_name: "records",
                limit: limits.max_records as u64,
                observed: limits.max_records as u64 + 1,
            });
        }
        file.seek(SeekFrom::Start(HEADER_SIZE as u64 + cursor))
            .map_err(|source| MmapScrollbackError::ReadRecord {
                path: path.to_path_buf(),
                source,
            })?;
        let mut record_bytes = [0u8; RECORD_HEADER_SIZE];
        file.read_exact(&mut record_bytes)
            .map_err(|source| MmapScrollbackError::ReadRecord {
                path: path.to_path_buf(),
                source,
            })?;
        if record_bytes == [0u8; RECORD_HEADER_SIZE] {
            terminal_reason = Some(LinearRecordTerminalReason::ZeroFilledRecordHeader);
            break;
        }
        let record = RecordHeader::decode(&record_bytes)?;
        let payload_end = cursor
            .checked_add(RECORD_HEADER_SIZE as u64)
            .and_then(|value| value.checked_add(u64::from(record.record_len)))
            .ok_or_else(|| MmapScrollbackError::UnsafeReadSource {
                path: path.to_path_buf(),
                reason: "record offset overflow",
            })?;
        if payload_end > header.write_cursor_bytes {
            terminal_reason = Some(LinearRecordTerminalReason::RecordPayloadPastDeclaredCursor);
            break;
        }
        // Never allocate for a payload that cannot physically fit in the file.
        // The payload occupies `[HEADER_SIZE + cursor + RECORD_HEADER_SIZE,
        // HEADER_SIZE + payload_end)`; if those bytes are not actually present
        // (truncated tail / corrupt oversized header) stop salvaging here rather
        // than reserving record_len bytes for data that isn't on disk.
        if (HEADER_SIZE as u64).saturating_add(payload_end) > file_len {
            terminal_reason = Some(LinearRecordTerminalReason::PhysicalRecordPayloadTruncated);
            break;
        }
        let next_payload_bytes = payload_bytes
            .checked_add(u64::from(record.record_len))
            .ok_or_else(|| MmapScrollbackError::ReadLimitExceeded {
                path: path.to_path_buf(),
                limit_name: "payload_bytes",
                limit: limits.max_payload_bytes,
                observed: u64::MAX,
            })?;
        if next_payload_bytes > limits.max_payload_bytes {
            return Err(MmapScrollbackError::ReadLimitExceeded {
                path: path.to_path_buf(),
                limit_name: "payload_bytes",
                limit: limits.max_payload_bytes,
                observed: next_payload_bytes,
            });
        }
        let mut payload = vec![0u8; record.record_len as usize];
        file.read_exact(&mut payload)
            .map_err(|source| MmapScrollbackError::ReadRecord {
                path: path.to_path_buf(),
                source,
            })?;
        records.push((record.record_kind, payload));
        payload_bytes = next_payload_bytes;
        cursor = payload_end;
    }

    let legacy_wrapped = header.total_bytes_written > header.write_cursor_bytes;
    let completeness = if legacy_wrapped {
        LinearRecordCompleteness::Incomplete {
            decoded_cursor_bytes: cursor,
            declared_cursor_bytes: header.write_cursor_bytes,
            reason: LinearRecordTerminalReason::LegacyWrappedOrderUnknown,
        }
    } else if cursor == header.write_cursor_bytes {
        LinearRecordCompleteness::Complete
    } else {
        LinearRecordCompleteness::Incomplete {
            decoded_cursor_bytes: cursor,
            declared_cursor_bytes: header.write_cursor_bytes,
            reason: terminal_reason.unwrap_or(
                LinearRecordTerminalReason::DeclaredTailTooShortForHeader,
            ),
        }
    };

    Ok(LinearRecordSnapshot {
        header,
        records,
        payload_bytes,
        source_identity,
        completeness,
    })
}

fn metadata_identity(metadata: &impl cap_fs_ext::MetadataExt) -> LinearRecordSourceIdentity {
    LinearRecordSourceIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn validate_private_cap_file(
    metadata: &cap_std::fs::Metadata,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    validate_cap_file_shape(metadata, path)?;
    #[cfg(unix)]
    if cap_std::fs::MetadataExt::mode(metadata) & 0o7177 != 0 {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "source permissions are not owner-private",
        });
    }
    Ok(())
}

fn validate_cap_file_shape(
    metadata: &cap_std::fs::Metadata,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "source is not one regular file with nlink=1",
        });
    }
    Ok(())
}

fn validate_private_cap_directory(
    metadata: &cap_std::fs::Metadata,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    validate_effective_uid_cap_directory(metadata, path)?;
    #[cfg(unix)]
    if cap_std::fs::MetadataExt::mode(metadata) & 0o7077 != 0 {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "scrollback directory permissions are not owner-private",
        });
    }
    Ok(())
}

fn validate_effective_uid_cap_directory(
    metadata: &cap_std::fs::Metadata,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    #[cfg(unix)]
    {
        validate_cap_directory_for_uid(metadata, path, rustix::process::geteuid().as_raw())
    }
    #[cfg(not(unix))]
    {
        if !metadata.is_dir() {
            return Err(MmapScrollbackError::UnsafeReadSource {
                path: path.to_path_buf(),
                reason: "scrollback security boundary is not a directory",
            });
        }
        Ok(())
    }
}

#[cfg(unix)]
fn validate_cap_directory_for_uid(
    metadata: &cap_std::fs::Metadata,
    path: &Path,
    effective_uid: u32,
) -> Result<(), MmapScrollbackError> {
    if !metadata.is_dir() {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "scrollback security boundary is not a directory",
        });
    }
    if cap_std::fs::MetadataExt::uid(metadata) != effective_uid {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "scrollback directory is not owned by the effective user",
        });
    }
    Ok(())
}

fn validate_effective_uid_std_directory(
    metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    if !metadata.is_dir() {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "opened scrollback security boundary is not a directory",
        });
    }
    #[cfg(unix)]
    if std::os::unix::fs::MetadataExt::uid(metadata) != rustix::process::geteuid().as_raw() {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "opened scrollback directory is not owned by the effective user",
        });
    }
    Ok(())
}

fn secure_writer_directory(
    directory: &CapDir,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    let before = directory.dir_metadata().map_err(|source| MmapScrollbackError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    validate_effective_uid_cap_directory(&before, path)?;
    let handle = directory
        .try_clone()
        .map_err(|source| MmapScrollbackError::Open {
            path: path.to_path_buf(),
            source,
        })?
        .into_std_file();
    let handle_before = handle.metadata().map_err(|source| MmapScrollbackError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    validate_effective_uid_std_directory(&handle_before, path)?;
    let identity = metadata_identity(&before);
    if metadata_identity(&handle_before) != identity {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "scrollback directory path and descriptor identities differ",
        });
    }

    #[cfg(unix)]
    {
        let path_mode = cap_std::fs::MetadataExt::mode(&before) & 0o7777;
        let handle_mode = std::os::unix::fs::MetadataExt::mode(&handle_before) & 0o7777;
        if path_mode != handle_mode {
            return Err(MmapScrollbackError::UnsafeReadSource {
                path: path.to_path_buf(),
                reason: "scrollback directory mode changed before permission migration",
            });
        }
        match path_mode {
            0o700 => {}
            0o755 => {
                use std::os::unix::fs::PermissionsExt as _;
                handle
                    .set_permissions(std::fs::Permissions::from_mode(0o700))
                    .map_err(|source| MmapScrollbackError::Permissions {
                        path: path.to_path_buf(),
                        mode: 0o700,
                        source,
                    })?;
            }
            _ => {
                return Err(MmapScrollbackError::UnsafeReadSource {
                    path: path.to_path_buf(),
                    reason: "scrollback directory mode is not 0700 or the exact migratable legacy mode 0755",
                });
            }
        }
    }
    handle.sync_all().map_err(|source| MmapScrollbackError::Sync {
        path: path.to_path_buf(),
        source,
    })?;

    let after = directory.dir_metadata().map_err(|source| MmapScrollbackError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    let handle_after = handle.metadata().map_err(|source| MmapScrollbackError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    validate_private_cap_directory(&after, path)?;
    validate_effective_uid_std_directory(&handle_after, path)?;
    #[cfg(unix)]
    if std::os::unix::fs::MetadataExt::mode(&handle_after) & 0o7077 != 0 {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "opened scrollback directory remains non-private after migration",
        });
    }
    if metadata_identity(&after) != identity || metadata_identity(&handle_after) != identity {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "scrollback directory identity changed during permission migration",
        });
    }
    let reopened = open_directory_tree_nofollow(path).map_err(|source| MmapScrollbackError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let reopened_metadata =
        reopened
            .dir_metadata()
            .map_err(|source| MmapScrollbackError::Metadata {
                path: path.to_path_buf(),
                source,
            })?;
    validate_private_cap_directory(&reopened_metadata, path)?;
    if metadata_identity(&reopened_metadata) != identity {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "scrollback directory path changed during permission migration",
        });
    }
    Ok(())
}

fn revalidate_writer_directory(
    directory: &CapDir,
    path: &Path,
    expected_identity: LinearRecordSourceIdentity,
) -> Result<(), MmapScrollbackError> {
    let pinned_metadata =
        directory
            .dir_metadata()
            .map_err(|source| MmapScrollbackError::Metadata {
                path: path.to_path_buf(),
                source,
            })?;
    validate_private_cap_directory(&pinned_metadata, path)?;
    let reopened = open_directory_tree_nofollow(path).map_err(|source| MmapScrollbackError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let named_metadata =
        reopened
            .dir_metadata()
            .map_err(|source| MmapScrollbackError::Metadata {
                path: path.to_path_buf(),
                source,
            })?;
    validate_private_cap_directory(&named_metadata, path)?;
    if metadata_identity(&pinned_metadata) != expected_identity
        || metadata_identity(&named_metadata) != expected_identity
    {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "scrollback directory identity changed during writer open",
        });
    }
    Ok(())
}

fn revalidate_writer_file(
    directory: &CapDir,
    leaf: &Path,
    display_path: &Path,
    file: &File,
    expected_identity: LinearRecordSourceIdentity,
) -> Result<(), MmapScrollbackError> {
    let directory_path = display_path.parent().unwrap_or_else(|| Path::new("."));
    let directory_metadata = directory.dir_metadata().map_err(|source| {
        MmapScrollbackError::Metadata {
            path: directory_path.to_path_buf(),
            source,
        }
    })?;
    validate_private_cap_directory(&directory_metadata, directory_path)?;
    let path_metadata = directory.symlink_metadata(leaf).map_err(|source| {
        MmapScrollbackError::Metadata {
            path: display_path.to_path_buf(),
            source,
        }
    })?;
    let handle_metadata = file.metadata().map_err(|source| MmapScrollbackError::Metadata {
        path: display_path.to_path_buf(),
        source,
    })?;
    validate_private_cap_file(&path_metadata, display_path)?;
    validate_private_std_file(&handle_metadata, display_path)?;
    validate_owner_against_directory(&path_metadata, &directory_metadata, display_path)?;
    validate_same_owner(&path_metadata, &handle_metadata, display_path)?;
    if metadata_identity(&path_metadata) != expected_identity
        || metadata_identity(&handle_metadata) != expected_identity
    {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: display_path.to_path_buf(),
            reason: "writer leaf changed identity after lock acquisition or open",
        });
    }
    Ok(())
}

fn validate_private_std_file(
    metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    validate_std_file_shape(metadata, path)?;
    #[cfg(unix)]
    if std::os::unix::fs::MetadataExt::mode(metadata) & 0o7177 != 0 {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "opened source permissions are not owner-private",
        });
    }
    Ok(())
}

fn validate_std_file_shape(
    metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "opened source is not one regular file with nlink=1",
        });
    }
    Ok(())
}

fn validate_same_owner(
    path_metadata: &cap_std::fs::Metadata,
    handle_metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    #[cfg(unix)]
    if cap_std::fs::MetadataExt::uid(path_metadata)
        != std::os::unix::fs::MetadataExt::uid(handle_metadata)
    {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "path and opened descriptor owners differ",
        });
    }
    #[cfg(not(unix))]
    let _ = (path_metadata, handle_metadata, path);
    Ok(())
}

fn validate_owner_against_directory(
    file_metadata: &cap_std::fs::Metadata,
    directory_metadata: &cap_std::fs::Metadata,
    path: &Path,
) -> Result<(), MmapScrollbackError> {
    #[cfg(unix)]
    if cap_std::fs::MetadataExt::uid(file_metadata)
        != cap_std::fs::MetadataExt::uid(directory_metadata)
    {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: path.to_path_buf(),
            reason: "source owner differs from its pinned directory owner",
        });
    }
    #[cfg(not(unix))]
    let _ = (file_metadata, directory_metadata, path);
    Ok(())
}

fn open_directory_tree_nofollow(path: &Path) -> std::io::Result<CapDir> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(std::io::Error::other(
            "scrollback path contains a parent component",
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

fn ensure_directory_tree_nofollow(path: &Path) -> std::io::Result<CapDir> {
    ensure_directory_tree_nofollow_with_sync(path, &mut |parent, identity| {
        sync_directory_for_publication(parent, identity)
    })
}

fn ensure_directory_tree_nofollow_with_sync<F>(
    path: &Path,
    sync_parent: &mut F,
) -> std::io::Result<CapDir>
where
    F: FnMut(&CapDir, LinearRecordSourceIdentity) -> std::io::Result<()>,
{
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(std::io::Error::other(
            "scrollback path contains a parent component",
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
    let parent = ensure_directory_tree_nofollow_with_sync(parent_path, sync_parent)?;
    let parent_identity = metadata_identity(&parent.dir_metadata()?);
    let publication_required = match parent.symlink_metadata(leaf) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error),
    };
    let mut builder = cap_std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    if publication_required {
        match parent.create_dir_with(leaf, &builder) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let child = parent.open_dir_nofollow(leaf)?;
    if publication_required {
        let child_identity = metadata_identity(&child.dir_metadata()?);
        sync_parent(&parent, parent_identity)?;
        let reopened_child = parent.open_dir_nofollow(leaf)?;
        if metadata_identity(&reopened_child.dir_metadata()?) != child_identity {
            return Err(std::io::Error::other(
                "new scrollback directory identity changed during publication",
            ));
        }
    }
    Ok(child)
}

fn sync_directory_for_publication(
    directory: &CapDir,
    expected_identity: LinearRecordSourceIdentity,
) -> std::io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()?;
    let metadata = directory.dir_metadata()?;
    if metadata_identity(&metadata) != expected_identity {
        return Err(std::io::Error::other(
            "directory identity changed during durable publication",
        ));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum MmapScrollbackError {
    #[error("scrollback mmap cap must be at least {minimum} bytes, got {actual}")]
    CapTooSmall { minimum: u64, actual: u64 },
    #[error("scrollback mmap cap must be at most {maximum} bytes, got {actual}")]
    CapTooLarge { maximum: u64, actual: u64 },
    #[error("legacy scrollback read limit {limit_name} must be finite and non-zero")]
    InvalidReadLimit { limit_name: &'static str },
    #[error(
        "legacy scrollback read limit {limit_name} exceeds hard maximum {maximum}: got {actual}"
    )]
    ReadLimitTooLarge {
        limit_name: &'static str,
        maximum: u64,
        actual: u64,
    },
    #[error(
        "legacy scrollback read limit {limit_name} exceeded for {path}: limit {limit}, observed {observed}"
    )]
    ReadLimitExceeded {
        path: PathBuf,
        limit_name: &'static str,
        limit: u64,
        observed: u64,
    },
    #[error("unsafe legacy scrollback read source {path}: {reason}")]
    UnsafeReadSource {
        path: PathBuf,
        reason: &'static str,
    },
    #[error(
        "existing scrollback file {path} does not match configured capacity {configured_capacity_bytes}: expected {expected_file_bytes} physical bytes, found {actual_file_bytes}; evidence was left untouched"
    )]
    ExistingFileShapeMismatch {
        path: PathBuf,
        configured_capacity_bytes: u64,
        expected_file_bytes: u64,
        actual_file_bytes: u64,
    },
    #[error(
        "legacy v1 scrollback file {path} is read-only; export or discard it explicitly instead of mutating it in place"
    )]
    LegacyV1ReadOnly { path: PathBuf },
    #[error("scrollback writer is already active for {path}")]
    WriterBusy { path: PathBuf },
    #[error("v2 scrollback integrity failure for {path}: {reason}")]
    V2Integrity {
        path: PathBuf,
        reason: &'static str,
    },
    #[error("failed to create scrollback mmap directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open scrollback mmap file {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to stat scrollback mmap file {path}: {source}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to set scrollback path {path} to mode {mode:o}: {source}")]
    Permissions {
        path: PathBuf,
        mode: u32,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to resize scrollback mmap file {path} to {len} bytes: {source}")]
    SetLen {
        path: PathBuf,
        len: u64,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to lock scrollback mmap file {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read scrollback mmap header {path}: {source}")]
    ReadHeader {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write scrollback mmap header {path}: {source}")]
    WriteHeader {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write scrollback mmap record {path}: {source}")]
    WriteRecord {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read scrollback mmap record {path}: {source}")]
    ReadRecord {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to sync scrollback mmap file {path}: {source}")]
    Sync {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("record payload {payload_bytes} bytes exceeds mmap capacity {capacity_bytes}")]
    RecordTooLarge {
        payload_bytes: usize,
        capacity_bytes: u64,
    },
    #[error(transparent)]
    Header(#[from] crate::scrollback_mmap_format::HeaderDecodeError),
    #[error(transparent)]
    Record(#[from] crate::scrollback_mmap_format::RecordDecodeError),
}

fn open_writer_file(
    directory: &CapDir,
    leaf: &Path,
    display_path: &Path,
) -> Result<OpenedWriterFile, MmapScrollbackError> {
    open_writer_file_with_sync(
        directory,
        leaf,
        display_path,
        &mut |parent, identity| sync_directory_for_publication(parent, identity),
    )
}

fn open_writer_file_with_sync<F>(
    directory: &CapDir,
    leaf: &Path,
    display_path: &Path,
    sync_parent: &mut F,
) -> Result<OpenedWriterFile, MmapScrollbackError>
where
    F: FnMut(&CapDir, LinearRecordSourceIdentity) -> std::io::Result<()>,
{
    let directory_path = display_path.parent().unwrap_or_else(|| Path::new("."));
    let directory_metadata = directory.dir_metadata().map_err(|source| {
        MmapScrollbackError::Metadata {
            path: directory_path.to_path_buf(),
            source,
        }
    })?;
    validate_private_cap_directory(&directory_metadata, directory_path)?;
    let directory_identity = metadata_identity(&directory_metadata);
    let publication_required = match directory.symlink_metadata(leaf) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(source) => {
            return Err(MmapScrollbackError::Metadata {
                path: display_path.to_path_buf(),
                source,
            });
        }
    };
    let mut options = CapOpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    if publication_required {
        // `create_new` is the authority boundary that distinguishes a leaf we
        // created from pre-existing evidence. If another actor wins the race,
        // fail closed instead of opening and initializing their bytes.
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = directory
        .open_with(leaf, &options)
        .map_err(|source| MmapScrollbackError::Open {
            path: display_path.to_path_buf(),
            source,
        })?
        .into_std();
    let path_metadata = directory.symlink_metadata(leaf).map_err(|source| {
        MmapScrollbackError::Metadata {
            path: display_path.to_path_buf(),
            source,
        }
    })?;
    let handle_metadata = file.metadata().map_err(|source| MmapScrollbackError::Metadata {
        path: display_path.to_path_buf(),
        source,
    })?;
    validate_cap_file_shape(&path_metadata, display_path)?;
    validate_std_file_shape(&handle_metadata, display_path)?;
    validate_owner_against_directory(&path_metadata, &directory_metadata, display_path)?;
    validate_same_owner(&path_metadata, &handle_metadata, display_path)?;
    let identity = metadata_identity(&handle_metadata);
    if metadata_identity(&path_metadata) != identity {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: display_path.to_path_buf(),
            reason: "writer path and descriptor identities differ",
        });
    }

    #[cfg(unix)]
    {
        let path_mode = cap_std::fs::MetadataExt::mode(&path_metadata) & 0o7777;
        let handle_mode = std::os::unix::fs::MetadataExt::mode(&handle_metadata) & 0o7777;
        if path_mode != handle_mode {
            return Err(MmapScrollbackError::UnsafeReadSource {
                path: display_path.to_path_buf(),
                reason: "writer leaf mode changed before permission migration",
            });
        }
        match path_mode {
            0o600 => {}
            0o644 => {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|source| MmapScrollbackError::Permissions {
                        path: display_path.to_path_buf(),
                        mode: 0o600,
                        source,
                    })?;
                file.sync_all().map_err(|source| MmapScrollbackError::Sync {
                    path: display_path.to_path_buf(),
                    source,
                })?;
            }
            _ => {
                return Err(MmapScrollbackError::UnsafeReadSource {
                    path: display_path.to_path_buf(),
                    reason: "writer leaf mode is not 0600 or the exact migratable legacy mode 0644",
                });
            }
        }
    }

    let path_metadata_after = directory.symlink_metadata(leaf).map_err(|source| {
        MmapScrollbackError::Metadata {
            path: display_path.to_path_buf(),
            source,
        }
    })?;
    let handle_metadata_after =
        file.metadata()
            .map_err(|source| MmapScrollbackError::Metadata {
                path: display_path.to_path_buf(),
                source,
            })?;
    validate_private_cap_file(&path_metadata_after, display_path)?;
    validate_private_std_file(&handle_metadata_after, display_path)?;
    validate_owner_against_directory(&path_metadata_after, &directory_metadata, display_path)?;
    validate_same_owner(&path_metadata_after, &handle_metadata_after, display_path)?;
    if metadata_identity(&path_metadata_after) != identity
        || metadata_identity(&handle_metadata_after) != identity
    {
        return Err(MmapScrollbackError::UnsafeReadSource {
            path: display_path.to_path_buf(),
            reason: "writer leaf identity changed during permission migration",
        });
    }
    if publication_required {
        file.sync_all().map_err(|source| MmapScrollbackError::Sync {
            path: display_path.to_path_buf(),
            source,
        })?;
        sync_parent(directory, directory_identity).map_err(|source| {
            MmapScrollbackError::Sync {
                path: directory_path.to_path_buf(),
                source,
            }
        })?;
        let directory_metadata_after = directory.dir_metadata().map_err(|source| {
            MmapScrollbackError::Metadata {
                path: directory_path.to_path_buf(),
                source,
            }
        })?;
        validate_private_cap_directory(&directory_metadata_after, directory_path)?;
        let published_path_metadata = directory.symlink_metadata(leaf).map_err(|source| {
            MmapScrollbackError::Metadata {
                path: display_path.to_path_buf(),
                source,
            }
        })?;
        let published_handle_metadata =
            file.metadata()
                .map_err(|source| MmapScrollbackError::Metadata {
                    path: display_path.to_path_buf(),
                    source,
                })?;
        if metadata_identity(&directory_metadata_after) != directory_identity
            || metadata_identity(&published_path_metadata) != identity
            || metadata_identity(&published_handle_metadata) != identity
        {
            return Err(MmapScrollbackError::UnsafeReadSource {
                path: display_path.to_path_buf(),
                reason: "writer leaf or parent identity changed during durable publication",
            });
        }
    }
    Ok(OpenedWriterFile {
        file,
        identity,
        created: publication_required,
    })
}

fn normalize_cap_bytes(cap_bytes: u64) -> Result<u64, MmapScrollbackError> {
    let minimum = V2_RECORD_HEADER_SIZE as u64 + 1;
    if cap_bytes < minimum {
        Err(MmapScrollbackError::CapTooSmall {
            minimum,
            actual: cap_bytes,
        })
    } else if cap_bytes > MAX_CAP_BYTES {
        Err(MmapScrollbackError::CapTooLarge {
            maximum: MAX_CAP_BYTES,
            actual: cap_bytes,
        })
    } else {
        Ok(cap_bytes)
    }
}

fn fit_payload_to_capacity(
    mut payload: Vec<u8>,
    capacity_bytes: u64,
) -> Result<Vec<u8>, MmapScrollbackError> {
    let max_payload = capacity_bytes.saturating_sub(V2_RECORD_HEADER_SIZE as u64);
    if max_payload == 0 {
        return Err(MmapScrollbackError::CapTooSmall {
            minimum: V2_RECORD_HEADER_SIZE as u64 + 1,
            actual: capacity_bytes,
        });
    }
    if payload.len() as u64 > max_payload {
        payload = payload[payload.len() - max_payload as usize..].to_vec();
    }
    Ok(payload)
}

fn pane_uuid_bytes(pane_uuid: &str) -> [u8; 32] {
    if pane_uuid.len() == 64
        && pane_uuid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        let mut bytes = [0u8; 32];
        if hex::decode_to_slice(pane_uuid, &mut bytes).is_ok() {
            return bytes;
        }
    }

    Sha256::digest(pane_uuid.as_bytes()).into()
}

fn pane_file_stem(pane_uuid: &str) -> String {
    hex::encode(pane_uuid_bytes(pane_uuid))
}

fn epoch_millis(instant: SystemTime) -> u64 {
    instant
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_dir(name: &str) -> PathBuf {
        let id = TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("ft-z4u60-{name}-{}-{id}", std::process::id()))
    }

    fn test_read_limits() -> LinearRecordReadLimits {
        LinearRecordReadLimits {
            max_file_bytes: 64 * 1024 * 1024,
            max_records: 4096,
            max_payload_bytes: 50 * 1024 * 1024,
        }
    }

    fn write_private(path: &Path, bytes: &[u8]) {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path).expect("open private test file");
        file.write_all(bytes).expect("write private test file");
    }

    fn create_private_dir(path: &Path) {
        std::fs::create_dir_all(path).expect("create dir");
        #[cfg(unix)]
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("secure test dir");
    }

    fn append_test_payload(
        writer: &mut MmapScrollback,
        kind: RecordKind,
        payload: &[u8],
    ) -> MmapAppendReport {
        writer
            .append_redacted_payload(
                kind,
                RedactionResult {
                    bytes: payload.to_vec(),
                    evidence: BytesRedactionEvidence {
                        original_input_bytes: payload.len() as u64,
                        decoded_input_text_bytes: payload.len() as u64,
                        redacted_output_bytes: payload.len() as u64,
                        ..BytesRedactionEvidence::default()
                    },
                },
            )
            .expect("append test payload")
    }

    /// Regression: a corrupt/oversized header must not drive an unbounded
    /// payload allocation. We hand-craft a 264-byte file whose header claims a
    /// ~5 GB write cursor and whose single record header claims a u32::MAX
    /// payload that is not on disk. Before the file-length guard,
    /// `read_linear_records` reserved `record_len` (~4 GB) via `vec![0u8; ..]`
    /// before `read_exact` could fail. The test completing (rather than
    /// OOM-aborting) is the assertion that the allocation is bounded; we also
    /// assert recovery salvages zero records and does not error.
    #[test]
    fn read_linear_records_corrupt_header_does_not_overallocate() {
        use crate::scrollback_mmap_format::{FormatVersion, HeaderFlags};

        let dir = test_dir("corrupt-overalloc");
        create_private_dir(&dir);
        let path = dir.join("corrupt.bin");

        let inflated = 5_000_000_000u64; // > u32::MAX, larger than any real body
        let header = ScrollbackHeader {
            version: FormatVersion::V1,
            flags: HeaderFlags::from_bits(0),
            capacity_bytes: inflated,
            write_cursor_bytes: inflated,
            pane_uuid: [0u8; 32],
            created_at_epoch_ms: 0,
            last_msync_at_epoch_ms: 0,
            redactions_applied: 0,
            total_bytes_written: 0,
        };

        let mut file_bytes = header.encode().to_vec();
        // A single record header claiming the maximum possible payload, with no
        // payload bytes following it.
        let record = RecordHeader {
            record_len: u32::MAX,
            record_kind: RecordKind::Text,
        };
        file_bytes.extend_from_slice(&record.encode());
        assert_eq!(file_bytes.len(), HEADER_SIZE + RECORD_HEADER_SIZE);

        write_private(&path, &file_bytes);

        let snapshot = read_linear_records(&path, test_read_limits())
            .expect("corrupt-but-parseable header should salvage, not error");
        assert!(
            snapshot.records.is_empty(),
            "no payload bytes are present, so nothing is salvageable"
        );
        assert_eq!(
            snapshot.completeness,
            LinearRecordCompleteness::Incomplete {
                decoded_cursor_bytes: 0,
                declared_cursor_bytes: inflated,
                reason: LinearRecordTerminalReason::PhysicalRecordPayloadTruncated,
            }
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second record whose declared length is included in the header's write
    /// cursor (so the existing `payload_end > write_cursor` check passes) but
    /// whose payload bytes were never flushed (torn write) is stopped at the
    /// file-length guard; the first, fully-written record is still salvaged.
    #[test]
    fn read_linear_records_truncated_tail_salvages_prefix() {
        use crate::scrollback_mmap_format::{FormatVersion, HeaderFlags};

        let dir = test_dir("truncated-tail");
        create_private_dir(&dir);
        let path = dir.join("torn.bin");

        let p1 = b"keep-me\n";
        let rec1 = RecordHeader {
            record_len: u32::try_from(p1.len()).unwrap(),
            record_kind: RecordKind::Text,
        };
        let r2_len: u32 = 1_000_000; // claimed-but-absent payload
        let rec2 = RecordHeader {
            record_len: r2_len,
            record_kind: RecordKind::Osc,
        };

        // The header's cursor accounts for BOTH records' headers + payloads,
        // exactly as it would after the writer bumped the cursor but crashed
        // before the second payload reached disk.
        let cursor = RECORD_HEADER_SIZE as u64
            + p1.len() as u64
            + RECORD_HEADER_SIZE as u64
            + u64::from(r2_len);
        let header = ScrollbackHeader {
            version: FormatVersion::V1,
            flags: HeaderFlags::from_bits(0),
            capacity_bytes: cursor,
            write_cursor_bytes: cursor,
            pane_uuid: [0u8; 32],
            created_at_epoch_ms: 0,
            last_msync_at_epoch_ms: 0,
            redactions_applied: 0,
            total_bytes_written: cursor,
        };

        // Physical file: header + rec1 header + rec1 payload + rec2 header, then
        // truncated (no rec2 payload on disk).
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(&rec1.encode());
        bytes.extend_from_slice(p1);
        bytes.extend_from_slice(&rec2.encode());
        write_private(&path, &bytes);

        let snapshot =
            read_linear_records(&path, test_read_limits()).expect("salvage prefix");
        assert_eq!(
            snapshot.records.len(),
            1,
            "only the fully-written record is salvageable"
        );
        assert_eq!(snapshot.records[0], (RecordKind::Text, p1.to_vec()));
        assert_eq!(
            snapshot.completeness,
            LinearRecordCompleteness::Incomplete {
                decoded_cursor_bytes: RECORD_HEADER_SIZE as u64 + p1.len() as u64,
                declared_cursor_bytes: cursor,
                reason: LinearRecordTerminalReason::PhysicalRecordPayloadTruncated,
            }
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_creates_mode_0600_file_and_sidecar_lock() {
        let dir = test_dir("mode");
        let writer =
            MmapScrollback::open(MmapScrollbackConfig::new(&dir, "pane-1").with_cap_bytes(4096))
                .expect("open writer");

        let expected_stem = pane_file_stem("pane-1");
        assert!(writer.path().ends_with(format!("{expected_stem}.bin")));
        assert!(
            writer
                .lock_path()
                .ends_with(format!("{expected_stem}.bin.lock"))
        );
        assert_eq!(hex::encode(writer.header().pane_uuid), expected_stem);
        assert_eq!(writer.header().capacity_bytes, 4096);

        #[cfg(unix)]
        {
            let data_mode = std::fs::metadata(writer.path())
                .expect("data metadata")
                .permissions()
                .mode()
                & 0o777;
            let lock_mode = std::fs::metadata(writer.lock_path())
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(data_mode, 0o600);
            assert_eq!(lock_mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn writer_safely_migrates_legacy_directory_and_leaf_modes() {
        let dir = test_dir("legacy-mode-migration");
        let config = MmapScrollbackConfig::new(&dir, "legacy-mode").with_cap_bytes(4096);
        drop(MmapScrollback::open(config.clone()).unwrap());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(config.bin_path(), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::set_permissions(config.lock_path(), std::fs::Permissions::from_mode(0o644))
            .unwrap();

        let writer = MmapScrollback::open(config).expect("migrate exact owned legacy paths");

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(writer.path()).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(writer.lock_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn writer_rejects_world_writable_directory_without_chmod_or_leaf_creation() {
        let dir = test_dir("reject-world-writable-directory");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let config =
            MmapScrollbackConfig::new(&dir, "unsafe-directory-mode").with_cap_bytes(4096);

        let error = MmapScrollback::open(config.clone()).unwrap_err();

        assert!(matches!(error, MmapScrollbackError::UnsafeReadSource { .. }));
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
            0o777
        );
        assert!(!config.bin_path().exists());
        assert!(!config.lock_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn writer_rejects_world_writable_leaves_before_chmod_or_content_mutation() {
        let lock_dir = test_dir("reject-world-writable-lock");
        create_private_dir(&lock_dir);
        let lock_config =
            MmapScrollbackConfig::new(&lock_dir, "unsafe-lock-mode").with_cap_bytes(4096);
        std::fs::write(lock_config.lock_path(), b"lock-content").unwrap();
        std::fs::set_permissions(
            lock_config.lock_path(),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();

        assert!(matches!(
            MmapScrollback::open(lock_config.clone()),
            Err(MmapScrollbackError::UnsafeReadSource { .. })
        ));
        assert_eq!(std::fs::read(lock_config.lock_path()).unwrap(), b"lock-content");
        assert_eq!(
            std::fs::metadata(lock_config.lock_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o666
        );

        let data_dir = test_dir("reject-world-writable-data");
        create_private_dir(&data_dir);
        let data_config =
            MmapScrollbackConfig::new(&data_dir, "unsafe-data-mode").with_cap_bytes(4096);
        std::fs::write(data_config.bin_path(), b"data-content").unwrap();
        std::fs::set_permissions(
            data_config.bin_path(),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();

        assert!(matches!(
            MmapScrollback::open(data_config.clone()),
            Err(MmapScrollbackError::UnsafeReadSource { .. })
        ));
        assert_eq!(std::fs::read(data_config.bin_path()).unwrap(), b"data-content");
        assert_eq!(
            std::fs::metadata(data_config.bin_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o666
        );
    }

    #[test]
    fn directory_creation_invokes_parent_publication_sync_seam_once_per_new_leaf() {
        let root = test_dir("directory-publication-sync");
        create_private_dir(&root);
        let nested = root.join("one").join("two");
        let mut sync_count = 0usize;
        let mut sync_parent = |parent: &CapDir, identity| {
            sync_count += 1;
            sync_directory_for_publication(parent, identity)
        };

        let directory = ensure_directory_tree_nofollow_with_sync(&nested, &mut sync_parent)
            .expect("create and durably publish nested directory");

        assert!(directory.dir_metadata().unwrap().is_dir());
        assert_eq!(sync_count, 2);

        let mut repeat_sync_count = 0usize;
        let mut repeat_sync_parent = |_: &CapDir, _| {
            repeat_sync_count += 1;
            Ok(())
        };
        ensure_directory_tree_nofollow_with_sync(&nested, &mut repeat_sync_parent).unwrap();
        assert_eq!(repeat_sync_count, 0, "existing names need no republish sync");
    }

    #[test]
    fn writer_leaf_creation_invokes_parent_publication_sync_seam_only_once() {
        let root = test_dir("leaf-publication-sync");
        create_private_dir(&root);
        let directory = open_directory_tree_nofollow(&root).unwrap();
        let leaf = Path::new("new-leaf.bin.lock");
        let display_path = root.join(leaf);
        let mut sync_count = 0usize;
        let mut sync_parent = |parent: &CapDir, identity| {
            sync_count += 1;
            sync_directory_for_publication(parent, identity)
        };

        let file = open_writer_file_with_sync(
            &directory,
            leaf,
            &display_path,
            &mut sync_parent,
        )
        .expect("create and durably publish writer leaf");
        drop(file);

        assert!(display_path.is_file());
        assert_eq!(sync_count, 1);

        let mut repeat_sync_count = 0usize;
        let mut repeat_sync_parent = |_: &CapDir, _| {
            repeat_sync_count += 1;
            Ok(())
        };
        drop(
            open_writer_file_with_sync(
                &directory,
                leaf,
                &display_path,
                &mut repeat_sync_parent,
            )
            .unwrap(),
        );
        assert_eq!(
            repeat_sync_count, 0,
            "an existing writer leaf needs no name-publication sync"
        );
    }

    #[cfg(unix)]
    #[test]
    fn writer_rejects_symlinked_final_directory() {
        use std::os::unix::fs::symlink;

        let target = test_dir("symlink-directory-target");
        create_private_dir(&target);
        let linked = test_dir("symlink-directory-leaf");
        symlink(&target, &linked).unwrap();

        let error = MmapScrollback::open(
            MmapScrollbackConfig::new(&linked, "symlink-directory").with_cap_bytes(4096),
        )
        .unwrap_err();

        assert!(matches!(error, MmapScrollbackError::CreateDir { .. }));
        let followed_target = MmapScrollbackConfig::new(&target, "symlink-directory");
        assert!(!followed_target.bin_path().exists());
        assert!(!followed_target.lock_path().exists());
    }

    #[test]
    fn writer_refuses_a_broad_directory_without_a_concrete_leaf() {
        for broad_path in [PathBuf::from("."), PathBuf::from(std::path::MAIN_SEPARATOR_STR)] {
            let error = MmapScrollback::open(
                MmapScrollbackConfig::new(broad_path.clone(), "broad-directory")
                    .with_cap_bytes(4096),
            )
            .unwrap_err();
            assert!(matches!(error, MmapScrollbackError::UnsafeReadSource { .. }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn writer_directory_authority_rejects_a_foreign_effective_uid() {
        let dir = test_dir("foreign-directory-owner");
        create_private_dir(&dir);
        let capability = open_directory_tree_nofollow(&dir).unwrap();
        let metadata = capability.dir_metadata().unwrap();
        let actual_uid = cap_std::fs::MetadataExt::uid(&metadata);
        let foreign_uid = actual_uid.checked_add(1).unwrap_or(actual_uid - 1);

        let error = validate_cap_directory_for_uid(&metadata, &dir, foreign_uid).unwrap_err();

        assert!(matches!(error, MmapScrollbackError::UnsafeReadSource { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn writer_never_repairs_a_symlink_or_hardlinked_leaf() {
        use std::os::unix::fs::symlink;

        let symlink_dir = test_dir("writer-symlink-leaf");
        create_private_dir(&symlink_dir);
        let symlink_config =
            MmapScrollbackConfig::new(&symlink_dir, "writer-symlink-leaf").with_cap_bytes(4096);
        let target = symlink_dir.join("outside-lock-target");
        write_private(&target, b"do-not-touch");
        symlink(&target, symlink_config.lock_path()).unwrap();

        assert!(MmapScrollback::open(symlink_config).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"do-not-touch");

        let hardlink_dir = test_dir("writer-hardlink-leaf");
        create_private_dir(&hardlink_dir);
        let hardlink_config =
            MmapScrollbackConfig::new(&hardlink_dir, "writer-hardlink-leaf").with_cap_bytes(4096);
        std::fs::write(hardlink_config.lock_path(), []).unwrap();
        std::fs::set_permissions(
            hardlink_config.lock_path(),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::hard_link(
            hardlink_config.lock_path(),
            hardlink_dir.join("second-lock-link"),
        )
        .unwrap();

        assert!(matches!(
            MmapScrollback::open(hardlink_config.clone()),
            Err(MmapScrollbackError::UnsafeReadSource { .. })
        ));
        assert_eq!(
            std::fs::metadata(hardlink_config.lock_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o644,
            "hardlinked leaf must be rejected before chmod"
        );
    }

    #[test]
    fn append_redacts_before_disk_write_and_updates_header() {
        let dir = test_dir("redact");
        let mut writer = MmapScrollback::open(
            MmapScrollbackConfig::new(&dir, "pane-secret")
                .with_cap_bytes(4096)
                .with_sync_every_appends(1),
        )
        .expect("open writer");
        let secret = b"token sk-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN\n";

        let report = writer
            .append(RecordKind::Text, secret)
            .expect("append secret");
        assert!(report.redaction.replacement_count > 0);
        assert_eq!(
            report.redaction.redacted_output_bytes,
            report.payload_bytes as u64
        );
        assert_eq!(report.redaction.original_input_bytes, secret.len() as u64);
        assert!(report.redaction.secret_input_bytes_replaced > 0);
        assert!(report.synced);

        let path = writer.path().to_path_buf();
        let header = writer.header();
        drop(writer);

        assert!(header.redactions_applied > 0);
        assert!(header.last_msync_at_epoch_ms > 0);
        let bytes = std::fs::read(&path).expect("read mmap file");
        assert!(!bytes.windows(3).any(|window| window == b"sk-"));
        assert!(
            bytes
                .windows(b"[REDACTED]".len())
                .any(|window| window == b"[REDACTED]")
        );
    }

    #[test]
    fn append_streaming_redacts_secret_split_across_appends() {
        let dir = test_dir("split-redact");
        let mut writer = MmapScrollback::open(
            MmapScrollbackConfig::new(&dir, "pane-split-secret")
                .with_cap_bytes(4096)
                .with_sync_every_appends(1),
        )
        .expect("open writer");
        let secret = b"sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRs";
        let split = 24;

        let first = writer
            .append(RecordKind::Text, &secret[..split])
            .expect("append first half");
        assert_eq!(first.payload_bytes, 0);

        let second_payload = [&secret[split..], b"\n".as_slice()].concat();
        let second = writer
            .append(RecordKind::Osc, &second_payload)
            .expect("append second half");
        assert!(second.redaction.replacement_count > 0);
        assert_eq!(
            first
                .redaction
                .original_input_bytes
                .saturating_add(second.redaction.original_input_bytes),
            (secret.len() + 1) as u64
        );
        assert_eq!(first.redaction.replacement_count, 0);
        assert!(second.redaction.secret_input_bytes_replaced >= secret.len() as u64);
        assert!(second.synced);

        let path = writer.path().to_path_buf();
        let header = writer.header();
        drop(writer);

        assert!(header.redactions_applied > 0);
        let bytes = std::fs::read(&path).expect("read mmap file");
        assert!(
            !bytes
                .windows(b"sk-ant-api03-".len())
                .any(|window| { window == b"sk-ant-api03-" })
        );
        assert!(
            bytes
                .windows(b"[REDACTED]".len())
                .any(|window| window == b"[REDACTED]")
        );
    }

    #[test]
    fn reopen_reads_previously_synced_linear_records() {
        let dir = test_dir("reopen");
        let config = MmapScrollbackConfig::new(&dir, "pane-reopen")
            .with_cap_bytes(4096)
            .with_sync_every_appends(1);
        let path = config.bin_path();

        {
            let mut writer = MmapScrollback::open(config.clone()).expect("open writer");
            writer
                .append(RecordKind::Text, b"first\n")
                .expect("append first");
            writer
                .append(RecordKind::Osc, b"\x1b]0;title\x07")
                .expect("append osc");
            writer.sync().expect("sync");
        }

        let snapshot = read_linear_records(&path, test_read_limits()).expect("read records");
        assert_eq!(snapshot.completeness, LinearRecordCompleteness::Complete);
        assert_eq!(snapshot.records.len(), 2);
        assert_eq!(snapshot.records[0], (RecordKind::Text, b"first\n".to_vec()));
        assert_eq!(
            snapshot.records[1],
            (RecordKind::Osc, b"\x1b]0;title\x07".to_vec())
        );
    }

    #[test]
    fn v2_wrap_preserves_surviving_records_in_logical_order() {
        let dir = test_dir("cap");
        let mut writer = MmapScrollback::open(
            MmapScrollbackConfig::new(&dir, "pane-cap")
                .with_cap_bytes(400)
                .with_sync_every_appends(1),
        )
        .expect("open writer");

        let a = vec![b'a'; 40];
        let b = vec![b'b'; 40];
        let c = vec![b'c'; 100];
        let d = vec![b'd'; 40];
        append_test_payload(&mut writer, RecordKind::Text, &a);
        append_test_payload(&mut writer, RecordKind::Osc, &b);
        append_test_payload(&mut writer, RecordKind::Csi, &c);
        append_test_payload(&mut writer, RecordKind::Clear, &d);

        assert_eq!(writer.v2_state.generation, 1);
        assert!(writer.header().write_cursor_bytes < 400);
        assert_eq!(
            std::fs::metadata(writer.path()).expect("metadata").len(),
            HEADER_SIZE as u64 + 400
        );
        let path = writer.path().to_path_buf();
        writer.sync().unwrap();
        drop(writer);
        let snapshot = read_linear_records(&path, test_read_limits()).unwrap();
        assert_eq!(snapshot.completeness, LinearRecordCompleteness::Complete);
        assert_eq!(
            snapshot.records,
            vec![
                (RecordKind::Osc, b),
                (RecordKind::Csi, c),
                (RecordKind::Clear, d),
            ]
        );
    }

    #[test]
    fn read_limits_fail_closed_for_file_records_and_payload() {
        let dir = test_dir("read-limits");
        let config = MmapScrollbackConfig::new(&dir, "bounded-read")
            .with_cap_bytes(4096)
            .with_sync_every_appends(1);
        let path = config.bin_path();
        let mut writer = MmapScrollback::open(config).unwrap();
        writer.append(RecordKind::Text, b"first").unwrap();
        writer.append(RecordKind::Text, b"second").unwrap();
        writer.sync().unwrap();
        drop(writer);

        let error = read_linear_records(
            &path,
            LinearRecordReadLimits {
                max_file_bytes: 1024,
                ..test_read_limits()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            MmapScrollbackError::ReadLimitExceeded {
                limit_name: "file_bytes",
                ..
            }
        ));

        let error = read_linear_records(
            &path,
            LinearRecordReadLimits {
                max_records: 1,
                ..test_read_limits()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            MmapScrollbackError::ReadLimitExceeded {
                limit_name: "records",
                ..
            }
        ));

        let error = read_linear_records(
            &path,
            LinearRecordReadLimits {
                max_payload_bytes: 5,
                ..test_read_limits()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            MmapScrollbackError::ReadLimitExceeded {
                limit_name: "payload_bytes",
                ..
            }
        ));
    }

    #[test]
    fn public_read_limits_reject_values_above_absolute_hard_caps() {
        for (limits, expected_name) in [
            (
                LinearRecordReadLimits {
                    max_file_bytes: HARD_MAX_LINEAR_RECORD_FILE_BYTES + 1,
                    ..test_read_limits()
                },
                "file_bytes",
            ),
            (
                LinearRecordReadLimits {
                    max_records: HARD_MAX_LINEAR_RECORDS + 1,
                    ..test_read_limits()
                },
                "records",
            ),
            (
                LinearRecordReadLimits {
                    max_file_bytes: HARD_MAX_LINEAR_RECORD_FILE_BYTES,
                    max_payload_bytes: HARD_MAX_LINEAR_RECORD_PAYLOAD_BYTES + 1,
                    ..test_read_limits()
                },
                "payload_bytes",
            ),
        ] {
            let error = limits.validate().unwrap_err();
            assert!(matches!(
                error,
                MmapScrollbackError::ReadLimitTooLarge { limit_name, .. }
                    if limit_name == expected_name
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn reader_rejects_symlink_hardlink_nonprivate_and_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let dir = test_dir("unsafe-reader-inputs");
        let config = MmapScrollbackConfig::new(&dir, "unsafe-reader").with_cap_bytes(4096);
        let path = config.bin_path();
        drop(MmapScrollback::open(config).unwrap());

        let symlink_path = dir.join("symlink.bin");
        symlink(&path, &symlink_path).unwrap();
        assert!(matches!(
            read_linear_records(&symlink_path, test_read_limits()),
            Err(MmapScrollbackError::UnsafeReadSource { .. })
                | Err(MmapScrollbackError::Open { .. })
                | Err(MmapScrollbackError::Metadata { .. })
        ));

        let hardlink_path = dir.join("hardlink.bin");
        std::fs::hard_link(&path, &hardlink_path).unwrap();
        assert!(matches!(
            read_linear_records(&path, test_read_limits()),
            Err(MmapScrollbackError::UnsafeReadSource { .. })
        ));

        let private_path = MmapScrollbackConfig::new(&dir, "nonprivate-reader").bin_path();
        write_private(&private_path, &ScrollbackHeader::new([7; 32], 4096, 0).encode());
        std::fs::set_permissions(&private_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            read_linear_records(&private_path, test_read_limits()),
            Err(MmapScrollbackError::UnsafeReadSource { .. })
        ));

        let linked_parent = dir.with_extension("linked-parent");
        symlink(&dir, &linked_parent).unwrap();
        assert!(read_linear_records(
            &linked_parent.join(private_path.file_name().unwrap()),
            test_read_limits(),
        )
        .is_err());
    }

    #[test]
    fn reader_detects_leaf_replacement_after_descriptor_open() {
        let dir = test_dir("replace-after-open");
        let config = MmapScrollbackConfig::new(&dir, "replace-after-open").with_cap_bytes(4096);
        let path = config.bin_path();
        drop(MmapScrollback::open(config).unwrap());
        let directory = open_directory_tree_nofollow(&dir).unwrap();
        let leaf = path.file_name().unwrap();
        let saved = dir.join("saved-original.bin");

        let result = read_linear_records_in_directory_with_hook(
            &directory,
            Path::new(leaf),
            &path,
            test_read_limits(),
            || {
                std::fs::rename(&path, &saved).unwrap();
                let bytes = std::fs::read(&saved).unwrap();
                write_private(&path, &bytes);
            },
        );

        assert!(matches!(
            result,
            Err(MmapScrollbackError::UnsafeReadSource { .. })
                | Err(MmapScrollbackError::Metadata { .. })
        ));
    }

    #[test]
    fn writer_rejects_existing_filename_header_identity_mismatch() {
        let dir = test_dir("writer-header-identity-mismatch");
        create_private_dir(&dir);
        let config =
            MmapScrollbackConfig::new(&dir, "configured-pane").with_cap_bytes(4096);
        let mismatched = ScrollbackHeader::new([0x7f; 32], 4096, 0).encode();
        write_private(&config.bin_path(), &mismatched);
        std::fs::OpenOptions::new()
            .write(true)
            .open(config.bin_path())
            .unwrap()
            .set_len(HEADER_SIZE as u64 + 4096)
            .unwrap();
        let before = std::fs::read(config.bin_path()).unwrap();

        let error = MmapScrollback::open(config.clone()).unwrap_err();

        assert!(matches!(error, MmapScrollbackError::UnsafeReadSource { .. }));
        assert_eq!(std::fs::read(config.bin_path()).unwrap(), before);
    }

    #[test]
    fn writer_rejects_preexisting_short_leaf_without_changing_one_byte() {
        let dir = test_dir("short-evidence-preserved");
        create_private_dir(&dir);
        let config =
            MmapScrollbackConfig::new(&dir, "short-evidence").with_cap_bytes(4096);
        write_private(&config.bin_path(), b"immutable forensic prefix");
        let before = std::fs::read(config.bin_path()).unwrap();

        let error = MmapScrollback::open(config.clone()).unwrap_err();

        assert!(matches!(
            error,
            MmapScrollbackError::ExistingFileShapeMismatch { .. }
        ));
        assert_eq!(std::fs::read(config.bin_path()).unwrap(), before);
    }

    #[test]
    fn writer_rejects_capacity_mismatch_without_resizing_or_reinitializing() {
        let dir = test_dir("capacity-evidence-preserved");
        let original =
            MmapScrollbackConfig::new(&dir, "capacity-evidence").with_cap_bytes(4096);
        let mut writer = MmapScrollback::open(original.clone()).unwrap();
        append_test_payload(&mut writer, RecordKind::Text, b"durable evidence");
        writer.sync().unwrap();
        drop(writer);
        let before = std::fs::read(original.bin_path()).unwrap();

        let mismatched = original.clone().with_cap_bytes(8192);
        let error = MmapScrollback::open(mismatched).unwrap_err();

        assert!(matches!(
            error,
            MmapScrollbackError::ExistingFileShapeMismatch { .. }
        ));
        assert_eq!(std::fs::read(original.bin_path()).unwrap(), before);
    }

    #[test]
    fn writer_rejects_exact_length_legacy_v1_without_in_place_migration() {
        let dir = test_dir("v1-read-only");
        create_private_dir(&dir);
        let config = MmapScrollbackConfig::new(&dir, "v1-read-only").with_cap_bytes(4096);
        let header = ScrollbackHeader::new(pane_uuid_bytes("v1-read-only"), 4096, 17);
        write_private(&config.bin_path(), &header.encode());
        std::fs::OpenOptions::new()
            .write(true)
            .open(config.bin_path())
            .unwrap()
            .set_len(HEADER_SIZE as u64 + 4096)
            .unwrap();
        let before = std::fs::read(config.bin_path()).unwrap();

        let error = MmapScrollback::open(config.clone()).unwrap_err();

        assert!(matches!(error, MmapScrollbackError::LegacyV1ReadOnly { .. }));
        assert_eq!(std::fs::read(config.bin_path()).unwrap(), before);
    }

    #[test]
    fn wrapped_v1_reader_never_claims_complete_logical_order() {
        let dir = test_dir("v1-wrap-incomplete");
        create_private_dir(&dir);
        let path = dir.join("wrapped-v1.bin");
        let payload = b"physical-prefix";
        let record = RecordHeader {
            record_len: payload.len() as u32,
            record_kind: RecordKind::Text,
        };
        let cursor = RECORD_HEADER_SIZE as u64 + payload.len() as u64;
        let mut header = ScrollbackHeader::new([0x44; 32], 128, 0);
        header.write_cursor_bytes = cursor;
        header.total_bytes_written = 128 + cursor;
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(&record.encode());
        bytes.extend_from_slice(payload);
        bytes.resize(HEADER_SIZE + 128, 0);
        write_private(&path, &bytes);

        let snapshot = read_linear_records(&path, test_read_limits()).unwrap();

        assert_eq!(snapshot.records, vec![(RecordKind::Text, payload.to_vec())]);
        assert_eq!(
            snapshot.completeness,
            LinearRecordCompleteness::Incomplete {
                decoded_cursor_bytes: cursor,
                declared_cursor_bytes: cursor,
                reason: LinearRecordTerminalReason::LegacyWrappedOrderUnknown,
            }
        );
    }

    #[test]
    fn post_flock_lock_leaf_replacement_fails_before_data_creation() {
        let dir = test_dir("replace-lock-after-flock");
        create_private_dir(&dir);
        let config =
            MmapScrollbackConfig::new(&dir, "replace-lock-after-flock").with_cap_bytes(4096);
        write_private(&config.lock_path(), b"original lock evidence");
        let saved_lock = dir.join("original-lock-preserved");
        let lock_path = config.lock_path();

        let error = MmapScrollback::open_with_after_lock(config.clone(), || {
            std::fs::rename(&lock_path, &saved_lock).unwrap();
            write_private(&lock_path, b"replacement lock evidence");
        })
        .unwrap_err();

        assert!(matches!(error, MmapScrollbackError::UnsafeReadSource { .. }));
        assert_eq!(std::fs::read(saved_lock).unwrap(), b"original lock evidence");
        assert_eq!(std::fs::read(lock_path).unwrap(), b"replacement lock evidence");
        assert!(!config.bin_path().exists());
    }

    #[test]
    fn second_writer_gets_bounded_busy_result() {
        let dir = test_dir("writer-busy");
        let config = MmapScrollbackConfig::new(&dir, "writer-busy").with_cap_bytes(4096);
        let first = MmapScrollback::open(config.clone()).unwrap();

        let error = MmapScrollback::open(config).unwrap_err();

        assert!(matches!(error, MmapScrollbackError::WriterBusy { .. }));
        drop(first);
    }

    #[test]
    fn configured_append_sync_cadence_has_no_hidden_per_append_sync() {
        let dir = test_dir("sync-cadence");
        let mut writer = MmapScrollback::open(
            MmapScrollbackConfig::new(&dir, "sync-cadence")
                .with_cap_bytes(4096)
                .with_sync_every_appends(3)
                .with_sync_interval(Duration::from_secs(3600)),
        )
        .unwrap();
        let sync_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        writer.sync_observer = Some(sync_count.clone());

        assert!(!append_test_payload(&mut writer, RecordKind::Text, b"one").synced);
        assert!(!append_test_payload(&mut writer, RecordKind::Text, b"two").synced);
        assert_eq!(sync_count.load(Ordering::SeqCst), 0);
        assert!(append_test_payload(&mut writer, RecordKind::Text, b"three").synced);
        assert_eq!(sync_count.load(Ordering::SeqCst), 1);
        assert!(!append_test_payload(&mut writer, RecordKind::Text, b"four").synced);
        assert_eq!(sync_count.load(Ordering::SeqCst), 1);
        writer.sync().unwrap();
        assert_eq!(sync_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn v2_record_digest_rejects_tamper_and_falls_back_incomplete() {
        let dir = test_dir("v2-record-tamper");
        let config = MmapScrollbackConfig::new(&dir, "v2-record-tamper")
            .with_cap_bytes(4096)
            .with_sync_every_appends(0)
            .with_sync_interval(Duration::from_secs(3600));
        let path = config.bin_path();
        let mut writer = MmapScrollback::open(config.clone()).unwrap();
        let first = b"first";
        let second = b"second";
        append_test_payload(&mut writer, RecordKind::Text, first);
        append_test_payload(&mut writer, RecordKind::Osc, second);
        writer.sync().unwrap();
        drop(writer);

        let second_payload_offset = HEADER_SIZE as u64
            + V2_RECORD_HEADER_SIZE as u64
            + first.len() as u64
            + V2_RECORD_HEADER_SIZE as u64;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(second_payload_offset)).unwrap();
        file.write_all(b"X").unwrap();
        file.sync_data().unwrap();
        drop(file);
        let tampered = std::fs::read(&path).unwrap();

        let snapshot = read_linear_records(&path, test_read_limits()).unwrap();
        assert_eq!(snapshot.records, vec![(RecordKind::Text, first.to_vec())]);
        assert!(matches!(
            snapshot.completeness,
            LinearRecordCompleteness::Incomplete {
                reason: LinearRecordTerminalReason::V2NewerStateInvalid,
                ..
            }
        ));
        assert!(matches!(
            MmapScrollback::open(config),
            Err(MmapScrollbackError::V2Integrity { .. })
        ));
        assert_eq!(std::fs::read(path).unwrap(), tampered);
    }

    #[test]
    fn writer_rejects_unbounded_capacity_configuration() {
        let dir = test_dir("cap-hard-limit");
        let error = MmapScrollback::open(
            MmapScrollbackConfig::new(&dir, "too-large")
                .with_cap_bytes(MAX_CAP_BYTES.saturating_add(1)),
        )
        .unwrap_err();

        assert!(matches!(error, MmapScrollbackError::CapTooLarge { .. }));
    }
}
