//! Crash-safe scrollback writer over the v1 mmap file format.
//!
//! This module intentionally keeps the public contract at the format
//! boundary: one per-pane `.bin` file plus a sidecar `.bin.lock`, fixed
//! header, tagged records, redaction-before-write, bounded capacity, and
//! explicit sync cadence. The crate forbids unsafe code, so the first
//! integration pass uses positional file writes and `sync_data` instead of
//! calling platform mmap APIs directly.

use crate::redactor::{BytesRedactionEvidence, RedactionResult, StreamingRedactor};
use crate::scrollback_mmap_format::{
    HEADER_SIZE, RECORD_HEADER_SIZE, RecordHeader, RecordKind, ScrollbackHeader,
};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_CAP_BYTES: u64 = 50 * 1024 * 1024;
const DEFAULT_SYNC_EVERY_APPENDS: u64 = 64;
const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_millis(250);

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
            .join(format!("{}.bin", sanitize_pane_uuid(&self.pane_uuid)))
    }

    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.base_dir
            .join(format!("{}.bin.lock", sanitize_pane_uuid(&self.pane_uuid)))
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
}

impl MmapScrollback {
    pub fn open(config: MmapScrollbackConfig) -> Result<Self, MmapScrollbackError> {
        let cap_bytes = normalize_cap_bytes(config.cap_bytes)?;
        fs::create_dir_all(&config.base_dir).map_err(|source| MmapScrollbackError::CreateDir {
            path: config.base_dir.clone(),
            source,
        })?;

        let path = config.bin_path();
        let lock_path = config.lock_path();
        let lock_file = open_lock_file(&lock_path)?;
        lock_file
            .lock_exclusive()
            .map_err(|source| MmapScrollbackError::Lock {
                path: lock_path.clone(),
                source,
            })?;

        let mut file = open_data_file(&path)?;
        let target_len = HEADER_SIZE as u64 + cap_bytes;
        let metadata_len = file
            .metadata()
            .map_err(|source| MmapScrollbackError::Metadata {
                path: path.clone(),
                source,
            })?
            .len();

        let header = if metadata_len >= HEADER_SIZE as u64 {
            let mut bytes = [0u8; HEADER_SIZE];
            file.seek(SeekFrom::Start(0))
                .and_then(|_| file.read_exact(&mut bytes))
                .map_err(|source| MmapScrollbackError::ReadHeader {
                    path: path.clone(),
                    source,
                })?;
            let decoded = ScrollbackHeader::decode(&bytes)?;
            if decoded.capacity_bytes != cap_bytes || metadata_len != target_len {
                file.set_len(target_len)
                    .map_err(|source| MmapScrollbackError::SetLen {
                        path: path.clone(),
                        len: target_len,
                        source,
                    })?;
                ScrollbackHeader {
                    capacity_bytes: cap_bytes,
                    write_cursor_bytes: decoded.write_cursor_bytes.min(cap_bytes),
                    ..decoded
                }
            } else {
                decoded
            }
        } else {
            file.set_len(target_len)
                .map_err(|source| MmapScrollbackError::SetLen {
                    path: path.clone(),
                    len: target_len,
                    source,
                })?;
            ScrollbackHeader::new(
                pane_uuid_bytes(&config.pane_uuid),
                cap_bytes,
                epoch_millis(SystemTime::now()),
            )
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
        };
        writer.write_header()?;
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
        let payload = fit_payload_to_capacity(redacted.bytes, self.header.capacity_bytes)?;
        let record_len =
            u32::try_from(payload.len()).map_err(|_| MmapScrollbackError::RecordTooLarge {
                payload_bytes: payload.len(),
                capacity_bytes: self.header.capacity_bytes,
            })?;
        let record = RecordHeader {
            record_len,
            record_kind,
        }
        .encode();
        let total_len = record.len() as u64 + payload.len() as u64;

        let start_cursor =
            if self.header.write_cursor_bytes + total_len > self.header.capacity_bytes {
                0
            } else {
                self.header.write_cursor_bytes
            };
        let offset = HEADER_SIZE as u64 + start_cursor;

        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.write_all(&record))
            .and_then(|_| self.file.write_all(&payload))
            .map_err(|source| MmapScrollbackError::WriteRecord {
                path: self.path.clone(),
                source,
            })?;

        self.header.write_cursor_bytes = (start_cursor + total_len) % self.header.capacity_bytes;
        self.header.total_bytes_written = self.header.total_bytes_written.saturating_add(total_len);
        self.header.redactions_applied = self
            .header
            .redactions_applied
            .saturating_add(u64::from(redacted.evidence.matches));
        self.write_header()?;

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
        self.header.last_msync_at_epoch_ms = epoch_millis(SystemTime::now());
        self.write_header()?;
        self.file
            .sync_data()
            .map_err(|source| MmapScrollbackError::Sync {
                path: self.path.clone(),
                source,
            })?;
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
            self.file
                .sync_data()
                .map_err(|source| MmapScrollbackError::Sync {
                    path: self.path.clone(),
                    source,
                })?;
            Ok(false)
        }
    }

    fn write_header(&mut self) -> Result<(), MmapScrollbackError> {
        let bytes = self.header.encode();
        self.file
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.file.write_all(&bytes))
            .map_err(|source| MmapScrollbackError::WriteHeader {
                path: self.path.clone(),
                source,
            })
    }
}

impl Drop for MmapScrollback {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

pub fn read_linear_records(path: &Path) -> Result<Vec<(RecordKind, Vec<u8>)>, MmapScrollbackError> {
    let mut file = File::open(path).map_err(|source| MmapScrollbackError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let mut header_bytes = [0u8; HEADER_SIZE];
    file.read_exact(&mut header_bytes)
        .map_err(|source| MmapScrollbackError::ReadHeader {
            path: path.to_path_buf(),
            source,
        })?;
    let header = ScrollbackHeader::decode(&header_bytes)?;
    let mut cursor = 0u64;
    let mut records = Vec::new();

    while cursor + RECORD_HEADER_SIZE as u64 <= header.write_cursor_bytes {
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
            break;
        }
        let record = RecordHeader::decode(&record_bytes)?;
        let payload_end = cursor + RECORD_HEADER_SIZE as u64 + u64::from(record.record_len);
        if payload_end > header.write_cursor_bytes {
            break;
        }
        let mut payload = vec![0u8; record.record_len as usize];
        file.read_exact(&mut payload)
            .map_err(|source| MmapScrollbackError::ReadRecord {
                path: path.to_path_buf(),
                source,
            })?;
        records.push((record.record_kind, payload));
        cursor = payload_end;
    }

    Ok(records)
}

#[derive(Debug, thiserror::Error)]
pub enum MmapScrollbackError {
    #[error("scrollback mmap cap must be at least {minimum} bytes, got {actual}")]
    CapTooSmall { minimum: u64, actual: u64 },
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

fn open_lock_file(path: &Path) -> Result<File, MmapScrollbackError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .map_err(|source| MmapScrollbackError::Open {
            path: path.to_path_buf(),
            source,
        })
}

fn open_data_file(path: &Path) -> Result<File, MmapScrollbackError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .map_err(|source| MmapScrollbackError::Open {
            path: path.to_path_buf(),
            source,
        })
}

fn normalize_cap_bytes(cap_bytes: u64) -> Result<u64, MmapScrollbackError> {
    let minimum = RECORD_HEADER_SIZE as u64 + 1;
    if cap_bytes < minimum {
        Err(MmapScrollbackError::CapTooSmall {
            minimum,
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
    let max_payload = capacity_bytes.saturating_sub(RECORD_HEADER_SIZE as u64);
    if max_payload == 0 {
        return Err(MmapScrollbackError::CapTooSmall {
            minimum: RECORD_HEADER_SIZE as u64 + 1,
            actual: capacity_bytes,
        });
    }
    if payload.len() as u64 > max_payload {
        payload = payload[payload.len() - max_payload as usize..].to_vec();
    }
    Ok(payload)
}

fn pane_uuid_bytes(pane_uuid: &str) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (idx, byte) in pane_uuid.as_bytes().iter().take(32).enumerate() {
        bytes[idx] = *byte;
    }
    bytes
}

fn sanitize_pane_uuid(pane_uuid: &str) -> String {
    pane_uuid
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
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
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_dir(name: &str) -> PathBuf {
        let id = TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("ft-z4u60-{name}-{}-{id}", std::process::id()))
    }

    #[test]
    fn writer_creates_mode_0600_file_and_sidecar_lock() {
        let dir = test_dir("mode");
        let writer =
            MmapScrollback::open(MmapScrollbackConfig::new(&dir, "pane-1").with_cap_bytes(4096))
                .expect("open writer");

        assert!(writer.path().ends_with("pane-1.bin"));
        assert!(writer.lock_path().ends_with("pane-1.bin.lock"));
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
        assert!(report.redaction.matches > 0);
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
        assert!(second.redaction.matches > 0);
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

        let records = read_linear_records(&path).expect("read records");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], (RecordKind::Text, b"first\n".to_vec()));
        assert_eq!(records[1], (RecordKind::Osc, b"\x1b]0;title\x07".to_vec()));
    }

    #[test]
    fn cap_is_honored_by_wrapping_cursor_inside_ring() {
        let dir = test_dir("cap");
        let mut writer = MmapScrollback::open(
            MmapScrollbackConfig::new(&dir, "pane-cap")
                .with_cap_bytes(32)
                .with_sync_every_appends(1),
        )
        .expect("open writer");

        writer
            .append(RecordKind::Text, b"1234567890")
            .expect("append one");
        writer
            .append(RecordKind::Text, b"abcdefghijklmnopqrstuvwxyz")
            .expect("append two");

        assert!(writer.header().write_cursor_bytes <= 32);
        assert_eq!(
            std::fs::metadata(writer.path()).expect("metadata").len(),
            HEADER_SIZE as u64 + 32
        );
    }
}
