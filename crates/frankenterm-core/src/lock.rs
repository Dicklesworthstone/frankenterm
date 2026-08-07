//! Single-instance lock for the watcher daemon.
//!
//! Uses OS-level file locking (via fs2) to ensure only one watcher instance
//! runs at a time. A sidecar metadata file records diagnostic information
//! for debugging.

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
#[cfg(any(unix, windows))]
use cap_fs_ext::MetadataExt as CapMetadataExt;
use cap_std::fs::{Dir as CapDir, Metadata as CapMetadata, OpenOptions as CapOpenOptions};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::SystemTime;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum admitted size of the small lock-holder metadata schema.
///
/// Reads are additionally bounded to this limit plus one byte so growth after
/// the initial metadata check cannot trigger unbounded allocation.
pub const MAX_LOCK_METADATA_BYTES: usize = 1024;
const LOCK_METADATA_READ_LIMIT: u64 = MAX_LOCK_METADATA_BYTES as u64 + 1;
const MAX_LOCK_METADATA_VERSION_BYTES: usize = 128;

// br-ft-zs9v0 / ft-interactive-systems-performance-4tenz.53:
// lock-holder metadata admission observability. Every unavailable, unsafe,
// oversized, changed, or invalid sidecar increments this counter. The
// structured warning contains only fixed phase/kind labels and bounded size
// information; it deliberately excludes raw paths, I/O errors, parser errors,
// and metadata contents.
//
// Same observability defect family as ft-iwg7x
// (robot_profile_bootstrap_serde_drop_count), ft-zkthg
// (workflows_serde_drop_count), ft-jyywz (audit_chain_export_dropped_count),
// ft-yygus (policy_decision_context_serde_drop_count), ft-rnpuc
// (mcp_clock_anomaly_count), ft-bn6qi (epoch_clock_anomaly_count),
// ft-ncijf (mcp_workflow_plan_serde_drop_count), ft-r3d4e
// (backup_manifest_parse_drop_count), and ft-jtcrv
// (ars_federation_payload_serde_drop_count).
static LOCK_METADATA_ADMISSION_FAILURE_COUNT: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of rejected or unavailable lock-holder metadata reads.
///
/// Each increment means a held lock could not be attributed to a concrete,
/// safely admitted [`LockMetadata`] record.
#[must_use]
pub fn lock_metadata_admission_failure_count() -> u64 {
    LOCK_METADATA_ADMISSION_FAILURE_COUNT.load(AtomicOrdering::Relaxed)
}

/// Test helper: reset the counter so regression tests can assert
/// post-bump values without state leakage between tests.
#[cfg(test)]
pub(crate) fn reset_lock_metadata_admission_failure_count_for_test() {
    LOCK_METADATA_ADMISSION_FAILURE_COUNT.store(0, AtomicOrdering::Relaxed);
}

#[cfg(test)]
static LOCK_METADATA_COUNTER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn lock_metadata_counter_test_lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK_METADATA_COUNTER_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[inline]
fn record_lock_metadata_parse_drop() {
    LOCK_METADATA_ADMISSION_FAILURE_COUNT.fetch_add(1, AtomicOrdering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LockMetadataReadFailure {
    phase: &'static str,
    kind: &'static str,
    observed_bytes: Option<u64>,
}

impl LockMetadataReadFailure {
    const fn new(phase: &'static str, kind: &'static str) -> Self {
        Self {
            phase,
            kind,
            observed_bytes: None,
        }
    }

    const fn with_size(phase: &'static str, kind: &'static str, observed_bytes: u64) -> Self {
        Self {
            phase,
            kind,
            observed_bytes: Some(observed_bytes),
        }
    }
}

fn stable_io_failure(phase: &'static str, error: &io::Error) -> LockMetadataReadFailure {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::InvalidData => "invalid_data",
        io::ErrorKind::WouldBlock => "would_block",
        _ => "io_unavailable",
    };
    LockMetadataReadFailure::new(phase, kind)
}

fn report_lock_metadata_read_failure(failure: LockMetadataReadFailure) {
    record_lock_metadata_parse_drop();
    let size_known = failure.observed_bytes.is_some();
    let observed_bytes = failure
        .observed_bytes
        .unwrap_or(0)
        .min(LOCK_METADATA_READ_LIMIT);
    tracing::warn!(
        target: "frankenterm::lock",
        event = "ft-interactive-systems-performance-4tenz.53",
        phase = failure.phase,
        kind = failure.kind,
        size_known,
        observed_bytes,
        max_bytes = MAX_LOCK_METADATA_BYTES as u64,
        "lock holder metadata was unavailable after bounded admission"
    );
}

fn split_metadata_path(meta_path: &Path) -> Option<(&Path, &Path)> {
    let name = meta_path.file_name()?;
    let parent = meta_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Some((parent, Path::new(name)))
}

fn admit_named_metadata(
    metadata: &CapMetadata,
    phase: &'static str,
) -> Result<(), LockMetadataReadFailure> {
    if metadata.file_type().is_symlink() {
        return Err(LockMetadataReadFailure::new(phase, "symlink"));
    }
    if !metadata.is_file() {
        return Err(LockMetadataReadFailure::new(phase, "not_regular_file"));
    }
    if metadata.len() > MAX_LOCK_METADATA_BYTES as u64 {
        return Err(LockMetadataReadFailure::with_size(
            phase,
            "oversized",
            metadata.len(),
        ));
    }
    Ok(())
}

fn metadata_observation_changed(left: &CapMetadata, right: &CapMetadata) -> bool {
    if left.len() != right.len() {
        return true;
    }
    matches!(
        (left.modified(), right.modified()),
        (Ok(left), Ok(right)) if left != right
    )
}

#[cfg(unix)]
fn same_metadata_identity(left: &CapMetadata, right: &CapMetadata) -> Option<bool> {
    Some(
        CapMetadataExt::dev(left) == CapMetadataExt::dev(right)
            && CapMetadataExt::ino(left) == CapMetadataExt::ino(right),
    )
}

#[cfg(windows)]
fn same_metadata_identity(left: &CapMetadata, right: &CapMetadata) -> Option<bool> {
    Some(
        CapMetadataExt::volume_serial_number(left)?
            == CapMetadataExt::volume_serial_number(right)?
            && CapMetadataExt::file_index(left)? == CapMetadataExt::file_index(right)?,
    )
}

#[cfg(not(any(unix, windows)))]
fn same_metadata_identity(_left: &CapMetadata, _right: &CapMetadata) -> Option<bool> {
    None
}

fn require_same_metadata_identity(
    left: &CapMetadata,
    right: &CapMetadata,
    phase: &'static str,
) -> Result<(), LockMetadataReadFailure> {
    match same_metadata_identity(left, right) {
        Some(true) => Ok(()),
        Some(false) => Err(LockMetadataReadFailure::new(
            phase,
            "namespace_identity_changed",
        )),
        None if cfg!(any(unix, windows)) => Err(LockMetadataReadFailure::new(
            phase,
            "identity_unavailable",
        )),
        None => Ok(()),
    }
}

fn read_lock_metadata_admitted_with_hook(
    meta_path: &Path,
    after_open: impl FnOnce(),
) -> Result<LockMetadata, LockMetadataReadFailure> {
    let (parent, name) = split_metadata_path(meta_path)
        .ok_or_else(|| LockMetadataReadFailure::new("path", "invalid_leaf"))?;
    let directory = CapDir::open_ambient_dir(parent, cap_std::ambient_authority())
        .map_err(|error| stable_io_failure("parent_open", &error))?;
    let named_before = directory
        .symlink_metadata(name)
        .map_err(|error| stable_io_failure("path_before_open", &error))?;
    admit_named_metadata(&named_before, "path_before_open")?;

    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(error) => {
            if let Ok(current) = directory.symlink_metadata(name) {
                if current.file_type().is_symlink() {
                    return Err(LockMetadataReadFailure::new("open", "symlink"));
                }
                if !current.is_file() {
                    return Err(LockMetadataReadFailure::new("open", "not_regular_file"));
                }
            }
            return Err(stable_io_failure("open", &error));
        }
    };
    let handle_before = file
        .metadata()
        .map_err(|error| stable_io_failure("handle_before_read", &error))?;
    admit_named_metadata(&handle_before, "handle_before_read")?;
    require_same_metadata_identity(&named_before, &handle_before, "open_identity")?;
    if metadata_observation_changed(&named_before, &handle_before) {
        return Err(LockMetadataReadFailure::new(
            "open_identity",
            "metadata_changed",
        ));
    }

    after_open();

    let mut raw = Vec::with_capacity(MAX_LOCK_METADATA_BYTES.saturating_add(1));
    (&mut file)
        .take(LOCK_METADATA_READ_LIMIT)
        .read_to_end(&mut raw)
        .map_err(|error| stable_io_failure("read", &error))?;

    let handle_after = file
        .metadata()
        .map_err(|error| stable_io_failure("handle_after_read", &error))?;
    admit_named_metadata(&handle_after, "handle_after_read")?;
    require_same_metadata_identity(&handle_before, &handle_after, "read_identity")?;
    if metadata_observation_changed(&handle_before, &handle_after) {
        return Err(LockMetadataReadFailure::new(
            "read_identity",
            "metadata_changed",
        ));
    }

    let named_after = directory
        .symlink_metadata(name)
        .map_err(|error| stable_io_failure("path_after_read", &error))?;
    admit_named_metadata(&named_after, "path_after_read")?;
    require_same_metadata_identity(&handle_after, &named_after, "path_after_read")?;
    if metadata_observation_changed(&handle_after, &named_after) {
        return Err(LockMetadataReadFailure::new(
            "path_after_read",
            "metadata_changed",
        ));
    }

    if raw.len() > MAX_LOCK_METADATA_BYTES {
        return Err(LockMetadataReadFailure::with_size(
            "read",
            "oversized",
            u64::try_from(raw.len()).unwrap_or(LOCK_METADATA_READ_LIMIT),
        ));
    }
    let metadata = serde_json::from_slice::<LockMetadata>(&raw).map_err(|_| {
        LockMetadataReadFailure::with_size(
            "parse",
            "invalid_schema",
            u64::try_from(raw.len()).unwrap_or(LOCK_METADATA_READ_LIMIT),
        )
    })?;
    if !metadata.is_admissible() {
        return Err(LockMetadataReadFailure::with_size(
            "validate",
            "invalid_schema",
            u64::try_from(raw.len()).unwrap_or(LOCK_METADATA_READ_LIMIT),
        ));
    }
    Ok(metadata)
}

fn read_lock_metadata_with_hook(
    meta_path: &Path,
    after_open: impl FnOnce(),
) -> Result<LockMetadata, LockMetadataReadFailure> {
    let result = read_lock_metadata_admitted_with_hook(meta_path, after_open);
    if let Err(failure) = result {
        report_lock_metadata_read_failure(failure);
    }
    result
}

/// Read and parse one lock-holder metadata sidecar through bounded, no-follow
/// admission. Failures are deliberately content-free and mean only that the
/// holder's identity is unknown; they never mean that the lock is free.
fn read_lock_metadata(meta_path: &Path) -> Result<LockMetadata, LockMetadataReadFailure> {
    read_lock_metadata_with_hook(meta_path, || {})
}

/// Errors that can occur during lock operations.
#[derive(Error, Debug)]
pub enum LockError {
    /// Lock is already held by another process.
    #[error("watcher already running (pid: {pid}, started: {started_at})")]
    AlreadyRunning { pid: u32, started_at: String },

    /// Lock is held but metadata is missing or corrupt.
    #[error("watcher already running (lock held, metadata unavailable)")]
    AlreadyRunningNoMeta,

    /// I/O error during lock operations.
    #[error("lock I/O error: {0}")]
    Io(#[from] io::Error),

    /// Failed to serialize/deserialize metadata.
    #[error("metadata error: {0}")]
    Metadata(#[from] serde_json::Error),

    /// Handoff sidecar contained an invalid protocol record.
    #[error("watcher handoff metadata invalid: {0}")]
    InvalidHandoff(#[from] WatcherHandoffError),
}

/// Versioned protocol marker for zero-downtime watcher handoff records.
pub const WATCHER_HANDOFF_PROTOCOL_VERSION: u32 = 1;

/// Validation failures for watcher handoff sidecar records.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum WatcherHandoffError {
    /// The sidecar came from an unsupported protocol version.
    #[error("unsupported protocol_version {got}, expected {expected}")]
    UnsupportedProtocolVersion { got: u32, expected: u32 },

    /// Generation zero is reserved for the pre-handoff single-watcher state.
    #[error("handoff generation must be greater than zero")]
    ZeroGeneration,

    /// A takeover cannot transfer ownership from a process to itself.
    #[error("predecessor and successor pid are both {pid}")]
    SameProcess { pid: u32 },

    /// Persisted cursors are event/segment ids and must not be negative.
    #[error("handoff cursor must be non-negative, got {cursor}")]
    NegativeCursor { cursor: i64 },
}

/// Lifecycle phase for the zero-downtime watcher handoff sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatcherHandoffPhase {
    /// Successor has created a handoff request and is waiting for the holder.
    Standby,
    /// Current holder should finish its tick, flush, and checkpoint.
    DrainRequested,
    /// Current holder has flushed and recorded the cursor/checkpoint boundary.
    Drained,
    /// Successor has taken the lock and resumed from the recorded boundary.
    TakeoverComplete,
    /// Handoff was abandoned; ordinary single-instance locking applies again.
    Aborted,
}

/// Versioned handoff record written next to the watcher lock.
///
/// This is intentionally separate from [`LockMetadata`]: the existing lock
/// holder metadata remains a simple contention diagnostic, while handoff state
/// carries the generation/cursor/checkpoint fields needed by the future
/// drain-and-takeover runtime wiring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherHandoffRecord {
    /// Protocol schema version for forward-compatible parsing.
    pub protocol_version: u32,
    /// Monotonic handoff generation. Generation 0 means no handoff.
    pub generation: u64,
    /// PID of the currently running watcher that must drain.
    pub predecessor_pid: u32,
    /// PID of the standby successor, if already known.
    pub successor_pid: Option<u32>,
    /// Last durable event/segment cursor at the drain boundary.
    pub cursor: Option<i64>,
    /// Millisecond timestamp when the predecessor checkpointed the boundary.
    pub checkpoint_ms: Option<u64>,
    /// Current handoff phase.
    pub phase: WatcherHandoffPhase,
}

impl WatcherHandoffRecord {
    /// Build a new drain request from a standby watcher.
    #[must_use]
    pub fn drain_requested(generation: u64, predecessor_pid: u32, successor_pid: u32) -> Self {
        Self {
            protocol_version: WATCHER_HANDOFF_PROTOCOL_VERSION,
            generation,
            predecessor_pid,
            successor_pid: Some(successor_pid),
            cursor: None,
            checkpoint_ms: None,
            phase: WatcherHandoffPhase::DrainRequested,
        }
    }

    /// Validate the sidecar before a watcher acts on it.
    pub fn validate(&self) -> Result<(), WatcherHandoffError> {
        if self.protocol_version != WATCHER_HANDOFF_PROTOCOL_VERSION {
            return Err(WatcherHandoffError::UnsupportedProtocolVersion {
                got: self.protocol_version,
                expected: WATCHER_HANDOFF_PROTOCOL_VERSION,
            });
        }
        if self.generation == 0 {
            return Err(WatcherHandoffError::ZeroGeneration);
        }
        if self
            .successor_pid
            .is_some_and(|successor| successor == self.predecessor_pid)
        {
            return Err(WatcherHandoffError::SameProcess {
                pid: self.predecessor_pid,
            });
        }
        if let Some(cursor) = self.cursor
            && cursor < 0
        {
            return Err(WatcherHandoffError::NegativeCursor { cursor });
        }
        Ok(())
    }
}

/// Diagnostic metadata written alongside the lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockMetadata {
    /// Process ID of the lock holder.
    pub pid: u32,
    /// Unix timestamp when the lock was acquired.
    pub started_at: u64,
    /// Human-readable start time.
    pub started_at_human: String,
    /// Version of wa that acquired the lock.
    pub wa_version: String,
}

impl LockMetadata {
    /// Create new metadata for the current process.
    fn new() -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        Self {
            pid: std::process::id(),
            started_at: now,
            started_at_human: chrono_lite_format(now),
            wa_version: crate::VERSION.to_string(),
        }
    }
}

/// Simple ISO-8601 timestamp formatting without chrono dependency.
fn chrono_lite_format(unix_secs: u64) -> String {
    // Very basic formatting - just use seconds since epoch with a note
    // In production you might want proper chrono, but this keeps deps minimal
    format!("unix:{unix_secs}")
}

/// An acquired single-instance lock.
///
/// The lock is automatically released when this guard is dropped.
#[derive(Debug)]
pub struct WatcherLock {
    _lock_file: File,
    lock_path: PathBuf,
    meta_path: PathBuf,
}

impl WatcherLock {
    /// Attempt to acquire the single-instance lock.
    ///
    /// Returns `Ok(WatcherLock)` if the lock was acquired successfully.
    /// Returns `Err(LockError::AlreadyRunning)` if another instance holds the lock.
    pub fn acquire(lock_path: &Path) -> Result<Self, LockError> {
        // Ensure parent directory exists
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Open or create the lock file
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;

        // Try to acquire exclusive lock (non-blocking)
        match lock_file.try_lock_exclusive() {
            Ok(()) => {
                // Lock acquired successfully
                let meta_path = metadata_path(lock_path);
                let lock = Self {
                    _lock_file: lock_file,
                    lock_path: lock_path.to_path_buf(),
                    meta_path,
                };
                lock.write_metadata()?;
                tracing::debug!(
                    lock_path = %lock_path.display(),
                    "Acquired watcher lock"
                );
                Ok(lock)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Lock is held by another process
                Err(read_existing_lock_error(lock_path))
            }
            Err(e) => Err(LockError::Io(e)),
        }
    }

    /// Write diagnostic metadata to the sidecar file.
    fn write_metadata(&self) -> Result<(), LockError> {
        let metadata = LockMetadata::new();
        let json = serde_json::to_string_pretty(&metadata)?;

        let mut file = File::create(&self.meta_path)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;

        tracing::debug!(
            meta_path = %self.meta_path.display(),
            pid = metadata.pid,
            "Wrote lock metadata"
        );
        Ok(())
    }

    /// Get the path to the lock file.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Get the path to the metadata file.
    #[must_use]
    pub fn meta_path(&self) -> &Path {
        &self.meta_path
    }
}

impl Drop for WatcherLock {
    fn drop(&mut self) {
        // Clean up metadata file on drop
        if let Err(e) = fs::remove_file(&self.meta_path) {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!(
                    meta_path = %self.meta_path.display(),
                    error = %e,
                    "Failed to remove lock metadata"
                );
            }
        }
        tracing::debug!(
            lock_path = %self.lock_path.display(),
            "Released watcher lock"
        );
        // Note: The actual file lock is released when _lock_file is dropped
    }
}

/// Compute the metadata sidecar path for a given lock path.
fn metadata_path(lock_path: &Path) -> PathBuf {
    let mut meta_path = lock_path.to_path_buf();
    let file_name = lock_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lock");
    meta_path.set_file_name(format!("{file_name}.meta.json"));
    meta_path
}

/// Compute the zero-downtime handoff sidecar path for a given lock path.
#[must_use]
pub fn watcher_handoff_path(lock_path: &Path) -> PathBuf {
    let mut path = lock_path.to_path_buf();
    let file_name = lock_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lock");
    path.set_file_name(format!("{file_name}.handoff.json"));
    path
}

/// Persist a watcher handoff record next to the lock file.
pub fn write_watcher_handoff_record(
    lock_path: &Path,
    record: &WatcherHandoffRecord,
) -> Result<PathBuf, LockError> {
    record.validate()?;
    let path = watcher_handoff_path(lock_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(record)?;
    let mut file = File::create(&path)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    Ok(path)
}

/// Read a watcher handoff record if the sidecar exists.
pub fn read_watcher_handoff_record(
    lock_path: &Path,
) -> Result<Option<WatcherHandoffRecord>, LockError> {
    let path = watcher_handoff_path(lock_path);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(LockError::Io(err)),
    };
    let record: WatcherHandoffRecord = serde_json::from_str(&raw)?;
    record.validate()?;
    Ok(Some(record))
}

/// Read metadata from an existing lock to provide a helpful error message.
fn read_existing_lock_error(lock_path: &Path) -> LockError {
    let meta_path = metadata_path(lock_path);
    match read_lock_metadata(&meta_path) {
        Some(meta) => LockError::AlreadyRunning {
            pid: meta.pid,
            started_at: meta.started_at_human,
        },
        None => LockError::AlreadyRunningNoMeta,
    }
}

/// Check if a watcher is currently running without acquiring the lock.
///
/// Returns `Some(metadata)` if the lock is held, `None` if it's free.
#[must_use]
pub fn check_running(lock_path: &Path) -> Option<LockMetadata> {
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(lock_path)
        .ok()?;

    // Try to acquire lock - if it fails, something is holding it
    match lock_file.try_lock_exclusive() {
        Ok(()) => {
            // We got the lock, so nothing was holding it
            // Release immediately by dropping the file handle
            drop(lock_file);
            None
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            // Lock is held, try to read metadata.
            // br-ft-zs9v0: route through read_lock_metadata so read-fail
            // and parse-fail bump LOCK_METADATA_PARSE_DROP_COUNT and emit
            // phase-tagged structured warns instead of silently
            // substituting None.
            let meta_path = metadata_path(lock_path);
            read_lock_metadata(&meta_path)
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_and_release_lock() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        // Acquire lock
        let lock = WatcherLock::acquire(&lock_path).unwrap();
        assert!(lock_path.exists());
        let meta_path = lock.meta_path().to_path_buf();
        assert!(meta_path.exists());

        // Drop releases lock and cleans up metadata
        drop(lock);
        assert!(!meta_path.exists());
    }

    #[test]
    fn double_acquire_fails() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        let _lock1 = WatcherLock::acquire(&lock_path).unwrap();

        // Second acquire should fail
        let result = WatcherLock::acquire(&lock_path);
        assert!(matches!(result, Err(LockError::AlreadyRunning { .. })));
    }

    #[test]
    fn check_running_detects_held_lock() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        // No lock yet
        assert!(check_running(&lock_path).is_none());

        let _lock = WatcherLock::acquire(&lock_path).unwrap();

        // Now lock is held
        let meta = check_running(&lock_path);
        assert!(meta.is_some());
        assert_eq!(meta.unwrap().pid, std::process::id());
    }

    #[test]
    fn metadata_contains_expected_fields() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        let lock = WatcherLock::acquire(&lock_path).unwrap();

        let meta_contents = fs::read_to_string(lock.meta_path()).unwrap();
        let meta: LockMetadata = serde_json::from_str(&meta_contents).unwrap();

        assert_eq!(meta.pid, std::process::id());
        assert!(!meta.wa_version.is_empty());
        assert!(meta.started_at > 0);
    }

    // ── Pure function tests ──

    #[test]
    fn lock_error_display_already_running() {
        let err = LockError::AlreadyRunning {
            pid: 12345,
            started_at: "unix:1700000000".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("12345"));
        assert!(msg.contains("unix:1700000000"));
    }

    #[test]
    fn lock_error_display_no_meta() {
        let err = LockError::AlreadyRunningNoMeta;
        assert!(err.to_string().contains("metadata unavailable"));
    }

    #[test]
    fn lock_error_display_io() {
        let err = LockError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        assert!(err.to_string().contains("denied"));
    }

    #[test]
    fn lock_error_display_metadata() {
        let json_err = serde_json::from_str::<LockMetadata>("not json").unwrap_err();
        let err = LockError::Metadata(json_err);
        assert!(err.to_string().contains("metadata error"));
    }

    #[test]
    fn lock_metadata_new_has_valid_fields() {
        let meta = LockMetadata::new();
        assert_eq!(meta.pid, std::process::id());
        assert!(meta.started_at > 0);
        assert!(meta.started_at_human.starts_with("unix:"));
        assert!(!meta.wa_version.is_empty());
    }

    #[test]
    fn lock_metadata_serde_roundtrip() {
        let meta = LockMetadata {
            pid: 999,
            started_at: 1_700_000_000,
            started_at_human: "unix:1700000000".to_string(),
            wa_version: "0.1.0".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, 999);
        assert_eq!(back.started_at, 1_700_000_000);
        assert_eq!(back.wa_version, "0.1.0");
    }

    #[test]
    fn chrono_lite_format_output() {
        assert_eq!(chrono_lite_format(0), "unix:0");
        assert_eq!(chrono_lite_format(1_700_000_000), "unix:1700000000");
    }

    #[test]
    fn metadata_path_appends_meta_json() {
        let path = PathBuf::from("/tmp/ft.lock");
        let meta = metadata_path(&path);
        assert_eq!(meta, PathBuf::from("/tmp/ft.lock.meta.json"));
    }

    #[test]
    fn metadata_path_handles_no_extension() {
        let path = PathBuf::from("/tmp/watcher");
        let meta = metadata_path(&path);
        assert_eq!(meta, PathBuf::from("/tmp/watcher.meta.json"));
    }

    #[test]
    fn watcher_handoff_path_appends_handoff_json() {
        let path = PathBuf::from("/tmp/watch.lock");
        let handoff = watcher_handoff_path(&path);
        assert_eq!(handoff, PathBuf::from("/tmp/watch.lock.handoff.json"));
    }

    #[test]
    fn watcher_handoff_drain_request_uses_protocol_version() {
        let record = WatcherHandoffRecord::drain_requested(7, 100, 200);
        assert_eq!(record.protocol_version, WATCHER_HANDOFF_PROTOCOL_VERSION);
        assert_eq!(record.generation, 7);
        assert_eq!(record.predecessor_pid, 100);
        assert_eq!(record.successor_pid, Some(200));
        assert_eq!(record.phase, WatcherHandoffPhase::DrainRequested);
        assert!(record.validate().is_ok());
    }

    #[test]
    fn watcher_handoff_validation_rejects_unsafe_records() {
        let mut record = WatcherHandoffRecord::drain_requested(1, 100, 200);
        record.protocol_version = WATCHER_HANDOFF_PROTOCOL_VERSION + 1;
        assert!(matches!(
            record.validate(),
            Err(WatcherHandoffError::UnsupportedProtocolVersion { .. })
        ));

        let mut record = WatcherHandoffRecord::drain_requested(1, 100, 200);
        record.generation = 0;
        assert!(matches!(
            record.validate(),
            Err(WatcherHandoffError::ZeroGeneration)
        ));

        let record = WatcherHandoffRecord::drain_requested(1, 100, 100);
        assert!(matches!(
            record.validate(),
            Err(WatcherHandoffError::SameProcess { pid: 100 })
        ));

        let mut record = WatcherHandoffRecord::drain_requested(1, 100, 200);
        record.cursor = Some(-1);
        assert!(matches!(
            record.validate(),
            Err(WatcherHandoffError::NegativeCursor { cursor: -1 })
        ));
    }

    #[test]
    fn watcher_handoff_record_roundtrip_preserves_boundary_fields() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        let mut record = WatcherHandoffRecord::drain_requested(3, 111, 222);
        record.phase = WatcherHandoffPhase::Drained;
        record.cursor = Some(42);
        record.checkpoint_ms = Some(1_700_000_123_456);

        let path = write_watcher_handoff_record(&lock_path, &record).unwrap();
        assert_eq!(path, watcher_handoff_path(&lock_path));

        let loaded = read_watcher_handoff_record(&lock_path)
            .unwrap()
            .expect("handoff record should exist");
        assert_eq!(loaded, record);
        assert_eq!(loaded.cursor, Some(42));
        assert_eq!(loaded.checkpoint_ms, Some(1_700_000_123_456));
    }

    #[test]
    fn watcher_handoff_read_missing_sidecar_is_none() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        assert!(read_watcher_handoff_record(&lock_path).unwrap().is_none());
    }

    #[test]
    fn watcher_handoff_read_rejects_invalid_sidecar() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        let handoff_path = watcher_handoff_path(&lock_path);
        fs::write(
            &handoff_path,
            r#"{
                "protocol_version": 1,
                "generation": 0,
                "predecessor_pid": 10,
                "successor_pid": 20,
                "cursor": null,
                "checkpoint_ms": null,
                "phase": "drain_requested"
            }"#,
        )
        .unwrap();

        assert!(matches!(
            read_watcher_handoff_record(&lock_path),
            Err(LockError::InvalidHandoff(
                WatcherHandoffError::ZeroGeneration
            ))
        ));
    }

    #[test]
    fn read_existing_lock_error_with_valid_meta() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");
        let meta_path = metadata_path(&lock_path);

        let meta = LockMetadata {
            pid: 42,
            started_at: 1234,
            started_at_human: "unix:1234".to_string(),
            wa_version: "0.1.0".to_string(),
        };
        let json = serde_json::to_string_pretty(&meta).unwrap();
        fs::write(&meta_path, json).unwrap();

        match read_existing_lock_error(&lock_path) {
            LockError::AlreadyRunning { pid, started_at } => {
                assert_eq!(pid, 42);
                assert_eq!(started_at, "unix:1234");
            }
            other => assert!(
                matches!(other, LockError::AlreadyRunning { .. }),
                "expected AlreadyRunning"
            ),
        }
    }

    #[test]
    fn read_existing_lock_error_with_corrupt_meta() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_lock_metadata_parse_drop_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");
        let meta_path = metadata_path(&lock_path);

        fs::write(&meta_path, "not valid json").unwrap();

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
        assert_eq!(lock_metadata_parse_drop_count(), 1);
    }

    #[test]
    fn read_existing_lock_error_no_meta_file() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_lock_metadata_parse_drop_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
        assert_eq!(lock_metadata_parse_drop_count(), 1);
    }

    #[test]
    fn lock_path_and_meta_path_accessors() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");
        let lock = WatcherLock::acquire(&lock_path).unwrap();

        assert_eq!(lock.lock_path(), lock_path);
        assert_eq!(lock.meta_path(), metadata_path(&lock_path));
    }

    #[test]
    fn reacquire_after_release() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        let lock1 = WatcherLock::acquire(&lock_path).unwrap();
        drop(lock1);

        // Should be reacquirable
        let lock2 = WatcherLock::acquire(&lock_path);
        assert!(lock2.is_ok());
    }

    #[test]
    fn check_running_no_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("nonexistent.lock");
        assert!(check_running(&lock_path).is_none());
    }

    #[test]
    fn acquire_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("sub").join("dir").join("test.lock");
        assert!(!tmp.path().join("sub").exists());

        let lock = WatcherLock::acquire(&lock_path).unwrap();
        assert!(lock_path.exists());
        drop(lock);
    }

    // ── Batch: RubyBeaver wa-1u90p.7.1 ──────────────────────────────────

    #[test]
    fn lock_error_variants_debug() {
        let already = LockError::AlreadyRunning {
            pid: 1,
            started_at: "now".to_string(),
        };
        let dbg = format!("{already:?}");
        assert!(dbg.contains("AlreadyRunning"));

        let no_meta = LockError::AlreadyRunningNoMeta;
        let dbg2 = format!("{no_meta:?}");
        assert!(dbg2.contains("AlreadyRunningNoMeta"));
    }

    #[test]
    fn lock_error_io_kind_preserved() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file gone");
        let lock_err = LockError::Io(io_err);
        let kind = match lock_err {
            LockError::Io(ref e) => Some(e.kind()),
            _ => None,
        };
        assert_eq!(kind, Some(io::ErrorKind::NotFound));
    }

    #[test]
    fn lock_metadata_debug() {
        let meta = LockMetadata::new();
        let dbg = format!("{meta:?}");
        assert!(dbg.contains("pid"));
        assert!(dbg.contains("started_at"));
        assert!(dbg.contains("wa_version"));
    }

    #[test]
    fn lock_metadata_clone() {
        let meta = LockMetadata {
            pid: 123,
            started_at: 456,
            started_at_human: "unix:456".to_string(),
            wa_version: "1.0".to_string(),
        };
        let cloned = meta.clone();
        assert_eq!(cloned.pid, 123);
        assert_eq!(cloned.started_at, 456);
    }

    #[test]
    fn chrono_lite_format_large_number() {
        let result = chrono_lite_format(u64::MAX);
        assert!(result.starts_with("unix:"));
        assert!(result.contains(&u64::MAX.to_string()));
    }

    #[test]
    fn chrono_lite_format_typical_epoch() {
        let result = chrono_lite_format(1_708_000_000);
        assert_eq!(result, "unix:1708000000");
    }

    #[test]
    fn metadata_path_with_dots_in_name() {
        let path = PathBuf::from("/tmp/my.watcher.lock");
        let meta = metadata_path(&path);
        assert_eq!(meta, PathBuf::from("/tmp/my.watcher.lock.meta.json"));
    }

    #[test]
    fn metadata_path_root_level() {
        let path = PathBuf::from("/lockfile");
        let meta = metadata_path(&path);
        assert_eq!(meta, PathBuf::from("/lockfile.meta.json"));
    }

    #[test]
    fn lock_file_persists_while_held() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("persist.lock");
        let lock = WatcherLock::acquire(&lock_path).unwrap();

        // Lock file should exist
        assert!(lock_path.exists());
        // Metadata file should exist
        assert!(lock.meta_path().exists());

        // Read metadata to verify content
        let contents = fs::read_to_string(lock.meta_path()).unwrap();
        let meta: LockMetadata = serde_json::from_str(&contents).unwrap();
        assert_eq!(meta.pid, std::process::id());

        drop(lock);
    }

    #[test]
    fn metadata_cleaned_up_after_drop() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("cleanup.lock");
        let meta_path;
        {
            let lock = WatcherLock::acquire(&lock_path).unwrap();
            meta_path = lock.meta_path().to_path_buf();
            assert!(meta_path.exists());
        }
        // After drop, metadata should be gone
        assert!(!meta_path.exists());
        // Lock file itself remains (it's just a file, the OS lock is released)
        assert!(lock_path.exists());
    }

    #[test]
    fn check_running_after_release_returns_none() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("released.lock");

        let lock = WatcherLock::acquire(&lock_path).unwrap();
        drop(lock);

        assert!(check_running(&lock_path).is_none());
    }

    #[test]
    fn double_acquire_error_includes_pid() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("double.lock");

        let _lock = WatcherLock::acquire(&lock_path).unwrap();
        let err = WatcherLock::acquire(&lock_path).unwrap_err();

        match err {
            LockError::AlreadyRunning { pid, started_at } => {
                assert_eq!(pid, std::process::id());
                assert!(started_at.starts_with("unix:"));
            }
            other => assert!(
                matches!(other, LockError::AlreadyRunning { .. }),
                "expected AlreadyRunning"
            ),
        }
    }

    #[test]
    fn lock_metadata_serde_with_special_chars() {
        let meta = LockMetadata {
            pid: 0,
            started_at: 0,
            started_at_human: "unix:0".to_string(),
            wa_version: "0.0.0-alpha+special\"chars".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.wa_version, "0.0.0-alpha+special\"chars");
    }

    #[test]
    fn lock_metadata_serde_empty_version() {
        let meta = LockMetadata {
            pid: 1,
            started_at: 1,
            started_at_human: "unix:1".to_string(),
            wa_version: String::new(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        assert!(back.wa_version.is_empty());
    }

    #[test]
    fn lock_metadata_pid_zero() {
        let meta = LockMetadata {
            pid: 0,
            started_at: 100,
            started_at_human: "unix:100".to_string(),
            wa_version: "test".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, 0);
    }

    #[test]
    fn lock_path_accessor_matches_input() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("accessor.lock");
        let lock = WatcherLock::acquire(&lock_path).unwrap();
        assert_eq!(lock.lock_path(), lock_path.as_path());
        drop(lock);
    }

    #[test]
    fn meta_path_accessor_matches_computed() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("meta-acc.lock");
        let lock = WatcherLock::acquire(&lock_path).unwrap();
        let expected_meta = metadata_path(&lock_path);
        assert_eq!(lock.meta_path(), expected_meta.as_path());
        drop(lock);
    }

    #[test]
    fn acquire_release_acquire_cycle() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("cycle.lock");

        for _ in 0..5 {
            let lock = WatcherLock::acquire(&lock_path).unwrap();
            assert!(lock_path.exists());
            drop(lock);
        }
    }

    #[test]
    fn read_existing_lock_error_empty_meta_file() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_lock_metadata_parse_drop_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("empty.lock");
        let meta_path = metadata_path(&lock_path);

        fs::write(&meta_path, "").unwrap();

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
        assert_eq!(lock_metadata_parse_drop_count(), 1);
    }

    #[test]
    fn read_existing_lock_error_partial_json() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_lock_metadata_parse_drop_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("partial.lock");
        let meta_path = metadata_path(&lock_path);

        fs::write(&meta_path, r#"{"pid": 42"#).unwrap();

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
        assert_eq!(lock_metadata_parse_drop_count(), 1);
    }

    #[test]
    fn check_running_with_stale_meta_but_no_lock() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("stale.lock");

        // Create and release a lock
        let lock = WatcherLock::acquire(&lock_path).unwrap();
        drop(lock);

        // Manually recreate a metadata file (simulating stale)
        let meta_path = metadata_path(&lock_path);
        let meta = LockMetadata {
            pid: 99999,
            started_at: 1,
            started_at_human: "unix:1".to_string(),
            wa_version: "old".to_string(),
        };
        fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        // check_running should return None because the lock is NOT held
        assert!(check_running(&lock_path).is_none());
    }

    #[test]
    fn lock_error_display_contains_io_message() {
        let err = LockError::Io(io::Error::other("custom error message"));
        assert!(err.to_string().contains("custom error message"));
    }
}

// br-ft-zs9v0: serialize tests that touch the process-global
// LOCK_METADATA_PARSE_DROP_COUNT counter so concurrent test threads
// don't race on reset/observe pairs.
#[cfg(test)]
mod metadata_parse_drop_tests {
    use super::*;
    use tempfile::TempDir;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        lock_metadata_counter_test_lock()
    }

    fn well_formed_metadata() -> LockMetadata {
        LockMetadata {
            pid: 1234,
            started_at: 1_700_000_000,
            started_at_human: "unix:1700000000".to_string(),
            wa_version: "0.0.0-test".to_string(),
        }
    }

    #[test]
    fn well_formed_metadata_does_not_bump() {
        let _g = lock();
        reset_lock_metadata_parse_drop_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.lock.meta");
        let raw = serde_json::to_string(&well_formed_metadata()).unwrap();
        fs::write(&path, raw).unwrap();
        let meta = read_lock_metadata(&path);
        assert!(meta.is_some());
        assert_eq!(meta.unwrap().pid, 1234);
        assert_eq!(lock_metadata_parse_drop_count(), 0);
    }

    #[test]
    fn missing_file_bumps_counter_via_read_fail() {
        let _g = lock();
        reset_lock_metadata_parse_drop_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does_not_exist.meta");
        let meta = read_lock_metadata(&path);
        assert!(meta.is_none());
        assert_eq!(lock_metadata_parse_drop_count(), 1);
    }

    #[test]
    fn malformed_json_bumps_counter_via_parse_fail() {
        let _g = lock();
        reset_lock_metadata_parse_drop_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.lock.meta");
        fs::write(&path, "{ not valid json").unwrap();
        let meta = read_lock_metadata(&path);
        assert!(meta.is_none());
        assert_eq!(lock_metadata_parse_drop_count(), 1);
    }

    #[test]
    fn wrong_shape_bumps_counter_via_parse_fail() {
        let _g = lock();
        reset_lock_metadata_parse_drop_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.lock.meta");
        // valid JSON but missing every required LockMetadata field
        fs::write(&path, r#"{"unrelated": true}"#).unwrap();
        let meta = read_lock_metadata(&path);
        assert!(meta.is_none());
        assert_eq!(lock_metadata_parse_drop_count(), 1);
    }

    #[test]
    fn repeated_failures_bump_monotonically() {
        let _g = lock();
        reset_lock_metadata_parse_drop_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.lock.meta");
        fs::write(&path, "garbage").unwrap();
        for _ in 0..5 {
            let _ = read_lock_metadata(&path);
        }
        assert_eq!(lock_metadata_parse_drop_count(), 5);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(48))]

        // br-ft-zs9v0: any non-LockMetadata-shaped JSON or non-JSON
        // content must bump the counter exactly once and yield None.
        #[test]
        fn arbitrary_malformed_content_always_bumps(
            shape in proptest::sample::select(vec![
                "null".to_string(),
                "true".to_string(),
                "42".to_string(),
                "\"string\"".to_string(),
                "[]".to_string(),
                "[1,2,3]".to_string(),
                "{}".to_string(),
                "{\"unknown\":42}".to_string(),
                "{\"pid\":\"not a number\"}".to_string(),
                "not json at all".to_string(),
                String::new(),
                "{".to_string(),
            ]),
        ) {
            let _g = lock();
            reset_lock_metadata_parse_drop_count_for_test();
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("watcher.lock.meta");
            fs::write(&path, &shape).unwrap();
            let meta = read_lock_metadata(&path);
            proptest::prop_assert!(meta.is_none());
            proptest::prop_assert_eq!(lock_metadata_parse_drop_count(), 1);
        }
    }
}
