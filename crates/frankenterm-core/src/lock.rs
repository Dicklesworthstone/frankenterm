//! Single-instance lock for the watcher daemon.
//!
//! Uses OS-level file locking (via fs2) to ensure only one watcher instance
//! runs at a time. A sidecar metadata file records diagnostic information
//! for debugging.

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
#[cfg(any(unix, windows))]
use cap_std::fs::MetadataExt as CapPlatformMetadataExt;
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt as CapOpenOptionsPlatformExt;
use cap_std::fs::{Dir as CapDir, Metadata as CapMetadata, OpenOptions as CapOpenOptions};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::SystemTime;

use fs2::FileExt;
use rand::{TryRng, rngs::SysRng};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum admitted size of the small lock-holder metadata schema.
///
/// Reads are additionally bounded to this limit plus one byte so growth after
/// the initial metadata check cannot trigger unbounded allocation.
pub const MAX_LOCK_METADATA_BYTES: usize = 1024;
const LOCK_METADATA_READ_LIMIT: u64 = MAX_LOCK_METADATA_BYTES as u64 + 1;
const MAX_LOCK_METADATA_VERSION_BYTES: usize = 128;
const LOCK_INSTANCE_ID_BYTES: usize = 16;
const LOCK_INSTANCE_ID_HEX_LEN: usize = LOCK_INSTANCE_ID_BYTES * 2;
/// Maximum admitted size of the watcher handoff protocol record.
pub const MAX_WATCHER_HANDOFF_BYTES: usize = 2048;

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
static WATCHER_HANDOFF_ADMISSION_FAILURE_COUNT: AtomicU64 = AtomicU64::new(0);
static LOCK_SIDECAR_WRITE_NONCE: AtomicU64 = AtomicU64::new(0);
const LOCK_SIDECAR_CREATE_ATTEMPTS: usize = 8;

/// Cumulative count of rejected or unavailable lock-holder metadata reads.
///
/// Each increment means a held lock could not be attributed to a concrete,
/// safely admitted [`LockMetadata`] record.
#[must_use]
pub fn lock_metadata_admission_failure_count() -> u64 {
    LOCK_METADATA_ADMISSION_FAILURE_COUNT.load(AtomicOrdering::Relaxed)
}

/// Cumulative count of rejected or unavailable watcher handoff reads.
#[must_use]
pub fn watcher_handoff_admission_failure_count() -> u64 {
    WATCHER_HANDOFF_ADMISSION_FAILURE_COUNT.load(AtomicOrdering::Relaxed)
}

/// Test helper: reset the counter so regression tests can assert
/// post-bump values without state leakage between tests.
#[cfg(test)]
pub(crate) fn reset_lock_metadata_admission_failure_count_for_test() {
    LOCK_METADATA_ADMISSION_FAILURE_COUNT.store(0, AtomicOrdering::Relaxed);
}

#[cfg(test)]
fn reset_watcher_handoff_admission_failure_count_for_test() {
    WATCHER_HANDOFF_ADMISSION_FAILURE_COUNT.store(0, AtomicOrdering::Relaxed);
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
fn record_lock_metadata_admission_failure() {
    LOCK_METADATA_ADMISSION_FAILURE_COUNT.fetch_add(1, AtomicOrdering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LockSidecarReadFailure {
    phase: &'static str,
    kind: &'static str,
    observed_bytes: Option<u64>,
}

impl LockSidecarReadFailure {
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

fn stable_io_failure(phase: &'static str, error: &io::Error) -> LockSidecarReadFailure {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::InvalidData => "invalid_data",
        io::ErrorKind::WouldBlock => "would_block",
        _ => "io_unavailable",
    };
    LockSidecarReadFailure::new(phase, kind)
}

fn is_lock_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    match (error.raw_os_error(), expected.raw_os_error()) {
        (Some(actual), Some(expected)) => actual == expected,
        _ => error.kind() == expected.kind(),
    }
}

fn require_external_advisory_lock(
    file: &File,
    phase: &'static str,
) -> Result<(), LockSidecarReadFailure> {
    match file.try_lock_exclusive() {
        Err(error) if is_lock_contended(&error) => Ok(()),
        Ok(()) => Err(LockSidecarReadFailure::new(
            phase,
            "holder_binding_missing",
        )),
        Err(error) => Err(stable_io_failure(phase, &error)),
    }
}

fn report_lock_metadata_read_failure(failure: LockSidecarReadFailure) {
    record_lock_metadata_admission_failure();
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

fn report_watcher_handoff_read_failure(failure: LockSidecarReadFailure) {
    WATCHER_HANDOFF_ADMISSION_FAILURE_COUNT.fetch_add(1, AtomicOrdering::Relaxed);
    let read_limit = u64::try_from(MAX_WATCHER_HANDOFF_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let size_known = failure.observed_bytes.is_some();
    let observed_bytes = failure.observed_bytes.unwrap_or(0).min(read_limit);
    tracing::warn!(
        target: "frankenterm::lock",
        event = "ft-interactive-systems-performance-4tenz.53",
        sidecar = "watcher_handoff",
        phase = failure.phase,
        kind = failure.kind,
        size_known,
        observed_bytes,
        max_bytes = MAX_WATCHER_HANDOFF_BYTES as u64,
        "watcher handoff record was unavailable after bounded admission"
    );
}

fn split_sidecar_path(sidecar_path: &Path) -> Option<(&Path, &Path)> {
    let name = sidecar_path.file_name()?;
    let parent = sidecar_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Some((parent, Path::new(name)))
}

fn admit_named_sidecar(
    metadata: &CapMetadata,
    max_bytes: usize,
    phase: &'static str,
) -> Result<(), LockSidecarReadFailure> {
    if metadata.file_type().is_symlink() {
        return Err(LockSidecarReadFailure::new(phase, "symlink"));
    }
    if !metadata.is_file() {
        return Err(LockSidecarReadFailure::new(phase, "not_regular_file"));
    }
    if metadata.len() > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
        return Err(LockSidecarReadFailure::with_size(
            phase,
            "oversized",
            metadata.len(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_observations_match(left: &CapMetadata, right: &CapMetadata) -> Option<bool> {
    Some(
        left.len() == right.len()
            && CapPlatformMetadataExt::mtime(left) == CapPlatformMetadataExt::mtime(right)
            && CapPlatformMetadataExt::mtime_nsec(left)
                == CapPlatformMetadataExt::mtime_nsec(right)
            && CapPlatformMetadataExt::ctime(left) == CapPlatformMetadataExt::ctime(right)
            && CapPlatformMetadataExt::ctime_nsec(left)
                == CapPlatformMetadataExt::ctime_nsec(right),
    )
}

#[cfg(windows)]
fn metadata_observations_match(left: &CapMetadata, right: &CapMetadata) -> Option<bool> {
    Some(
        left.len() == right.len()
            && CapPlatformMetadataExt::file_attributes(left)
                == CapPlatformMetadataExt::file_attributes(right)
            && CapPlatformMetadataExt::creation_time(left)
                == CapPlatformMetadataExt::creation_time(right)
            && CapPlatformMetadataExt::last_write_time(left)
                == CapPlatformMetadataExt::last_write_time(right),
    )
}

#[cfg(not(any(unix, windows)))]
fn metadata_observations_match(left: &CapMetadata, right: &CapMetadata) -> Option<bool> {
    match (left.modified(), right.modified()) {
        (Ok(left_modified), Ok(right_modified)) => {
            Some(left.len() == right.len() && left_modified == right_modified)
        }
        _ => None,
    }
}

fn require_same_metadata_observation(
    left: &CapMetadata,
    right: &CapMetadata,
    phase: &'static str,
) -> Result<(), LockSidecarReadFailure> {
    match metadata_observations_match(left, right) {
        Some(true) => Ok(()),
        Some(false) => Err(LockSidecarReadFailure::new(phase, "metadata_changed")),
        None => Err(LockSidecarReadFailure::new(
            phase,
            "timestamp_unavailable",
        )),
    }
}

#[cfg(unix)]
fn same_metadata_identity(left: &CapMetadata, right: &CapMetadata) -> Option<bool> {
    Some(
        CapPlatformMetadataExt::dev(left) == CapPlatformMetadataExt::dev(right)
            && CapPlatformMetadataExt::ino(left) == CapPlatformMetadataExt::ino(right),
    )
}

#[cfg(windows)]
fn same_metadata_identity(left: &CapMetadata, right: &CapMetadata) -> Option<bool> {
    let left_volume = CapPlatformMetadataExt::volume_serial_number(left)?;
    let right_volume = CapPlatformMetadataExt::volume_serial_number(right)?;
    let left_index = CapPlatformMetadataExt::file_index(left)?;
    let right_index = CapPlatformMetadataExt::file_index(right)?;
    Some(left_volume == right_volume && left_index == right_index)
}

#[cfg(not(any(unix, windows)))]
fn same_metadata_identity(_left: &CapMetadata, _right: &CapMetadata) -> Option<bool> {
    None
}

fn require_same_metadata_identity(
    left: &CapMetadata,
    right: &CapMetadata,
    phase: &'static str,
) -> Result<(), LockSidecarReadFailure> {
    match same_metadata_identity(left, right) {
        Some(true) => Ok(()),
        Some(false) => Err(LockSidecarReadFailure::new(
            phase,
            "namespace_identity_changed",
        )),
        None if cfg!(any(unix, windows)) => Err(LockSidecarReadFailure::new(
            phase,
            "identity_unavailable",
        )),
        None => Ok(()),
    }
}

struct AdmittedSidecarRead {
    file: File,
    observed_metadata: CapMetadata,
    raw: Vec<u8>,
}

fn read_lock_sidecar_bytes_in_directory_admitted_with_hook(
    directory: &CapDir,
    name: &Path,
    max_bytes: usize,
    require_holder_binding: bool,
    after_open: impl FnOnce(),
) -> Result<AdmittedSidecarRead, LockSidecarReadFailure> {
    let named_before = directory
        .symlink_metadata(name)
        .map_err(|error| stable_io_failure("path_before_open", &error))?;
    admit_named_sidecar(&named_before, max_bytes, "path_before_open")?;

    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    if require_holder_binding {
        options.write(true);
    }
    let cap_file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(error) => {
            if let Ok(current) = directory.symlink_metadata(name) {
                if current.file_type().is_symlink() {
                    return Err(LockSidecarReadFailure::new("open", "symlink"));
                }
                if !current.is_file() {
                    return Err(LockSidecarReadFailure::new("open", "not_regular_file"));
                }
            }
            return Err(stable_io_failure("open", &error));
        }
    };
    let mut file = cap_file.into_std();
    let handle_before = CapMetadata::from_file(&file)
        .map_err(|error| stable_io_failure("handle_before_read", &error))?;
    admit_named_sidecar(&handle_before, max_bytes, "handle_before_read")?;
    require_same_metadata_identity(&named_before, &handle_before, "open_identity")?;
    require_same_metadata_observation(&named_before, &handle_before, "open_identity")?;
    if require_holder_binding {
        require_external_advisory_lock(&file, "holder_binding_before_read")?;
    }

    after_open();

    let read_limit = u64::try_from(max_bytes).unwrap_or(u64::MAX).saturating_add(1);
    let mut raw = Vec::with_capacity(max_bytes.saturating_add(1));
    (&mut file)
        .take(read_limit)
        .read_to_end(&mut raw)
        .map_err(|error| stable_io_failure("read", &error))?;

    let handle_after = CapMetadata::from_file(&file)
        .map_err(|error| stable_io_failure("handle_after_read", &error))?;
    admit_named_sidecar(&handle_after, max_bytes, "handle_after_read")?;
    require_same_metadata_identity(&handle_before, &handle_after, "read_identity")?;
    require_same_metadata_observation(&handle_before, &handle_after, "read_identity")?;

    let named_after = directory
        .symlink_metadata(name)
        .map_err(|error| stable_io_failure("path_after_read", &error))?;
    admit_named_sidecar(&named_after, max_bytes, "path_after_read")?;
    require_same_metadata_identity(&handle_after, &named_after, "path_after_read")?;
    require_same_metadata_observation(&handle_after, &named_after, "path_after_read")?;
    if require_holder_binding {
        require_external_advisory_lock(&file, "holder_binding_after_read")?;
    }

    if raw.len() > max_bytes {
        return Err(LockSidecarReadFailure::with_size(
            "read",
            "oversized",
            u64::try_from(raw.len()).unwrap_or(read_limit),
        ));
    }
    Ok(AdmittedSidecarRead {
        file,
        observed_metadata: handle_after,
        raw,
    })
}

fn read_lock_sidecar_bytes_admitted_with_hook(
    sidecar_path: &Path,
    max_bytes: usize,
    after_open: impl FnOnce(),
) -> Result<Vec<u8>, LockSidecarReadFailure> {
    let (parent, name) = split_sidecar_path(sidecar_path)
        .ok_or_else(|| LockSidecarReadFailure::new("path", "invalid_leaf"))?;
    let directory = CapDir::open_ambient_dir(parent, cap_std::ambient_authority())
        .map_err(|error| stable_io_failure("parent_open", &error))?;
    read_lock_sidecar_bytes_in_directory_admitted_with_hook(
        &directory,
        name,
        max_bytes,
        false,
        after_open,
    )
    .map(|admitted| admitted.raw)
}

fn decode_lock_metadata(raw: &[u8]) -> Result<LockMetadata, LockSidecarReadFailure> {
    let metadata = serde_json::from_slice::<LockMetadata>(raw).map_err(|_| {
        LockSidecarReadFailure::with_size(
            "parse",
            "invalid_schema",
            u64::try_from(raw.len()).unwrap_or(LOCK_METADATA_READ_LIMIT),
        )
    })?;
    if !metadata.is_admissible() {
        return Err(LockSidecarReadFailure::with_size(
            "validate",
            "invalid_schema",
            u64::try_from(raw.len()).unwrap_or(LOCK_METADATA_READ_LIMIT),
        ));
    }
    Ok(metadata)
}

#[cfg(test)]
fn read_lock_metadata_admitted_with_hook(
    meta_path: &Path,
    after_open: impl FnOnce(),
) -> Result<LockMetadata, LockSidecarReadFailure> {
    let raw = read_lock_sidecar_bytes_admitted_with_hook(
        meta_path,
        MAX_LOCK_METADATA_BYTES,
        after_open,
    )?;
    decode_lock_metadata(&raw)
}

#[cfg(test)]
fn read_lock_metadata_with_hook(
    meta_path: &Path,
    after_open: impl FnOnce(),
) -> Result<LockMetadata, LockSidecarReadFailure> {
    let result = read_lock_metadata_admitted_with_hook(meta_path, after_open);
    if let Err(failure) = &result {
        report_lock_metadata_read_failure(*failure);
    }
    result
}

/// Read and parse one lock-holder metadata sidecar through bounded, no-follow
/// admission. Failures are deliberately content-free and mean only that the
/// bytes are not admissible. This test-only helper proves schema admission,
/// not holder authority; production attribution additionally requires the
/// exact sidecar inode's advisory lock.
#[cfg(test)]
fn read_lock_metadata(meta_path: &Path) -> Result<LockMetadata, LockSidecarReadFailure> {
    read_lock_metadata_with_hook(meta_path, || {})
}

fn read_lock_metadata_in_directory_retained(
    directory: &CapDir,
    name: &Path,
) -> Result<(LockMetadata, AdmittedSidecarRead), LockSidecarReadFailure> {
    let admitted = read_lock_sidecar_bytes_in_directory_admitted_with_hook(
        directory,
        name,
        MAX_LOCK_METADATA_BYTES,
        true,
        || {},
    )?;
    let metadata = decode_lock_metadata(&admitted.raw)?;
    Ok((metadata, admitted))
}

fn verify_retained_lock_metadata_authority(
    directory: &CapDir,
    name: &Path,
    admitted: &AdmittedSidecarRead,
) -> Result<(), LockSidecarReadFailure> {
    let handle_final = CapMetadata::from_file(&admitted.file)
        .map_err(|error| stable_io_failure("handle_final_authority", &error))?;
    admit_named_sidecar(
        &handle_final,
        MAX_LOCK_METADATA_BYTES,
        "handle_final_authority",
    )?;
    require_same_metadata_identity(
        &admitted.observed_metadata,
        &handle_final,
        "handle_final_authority",
    )?;
    require_same_metadata_observation(
        &admitted.observed_metadata,
        &handle_final,
        "handle_final_authority",
    )?;

    let named_final = directory
        .symlink_metadata(name)
        .map_err(|error| stable_io_failure("path_final_authority", &error))?;
    admit_named_sidecar(
        &named_final,
        MAX_LOCK_METADATA_BYTES,
        "path_final_authority",
    )?;
    require_same_metadata_identity(&handle_final, &named_final, "path_final_authority")?;
    require_same_metadata_observation(&handle_final, &named_final, "path_final_authority")?;
    require_external_advisory_lock(&admitted.file, "holder_binding_final")
}

fn admit_regular_leaf(
    metadata: &CapMetadata,
    phase: &'static str,
) -> Result<(), LockSidecarReadFailure> {
    if metadata.file_type().is_symlink() {
        return Err(LockSidecarReadFailure::new(phase, "symlink"));
    }
    if !metadata.is_file() {
        return Err(LockSidecarReadFailure::new(phase, "not_regular_file"));
    }
    Ok(())
}

struct OpenedLockLeaf {
    directory: CapDir,
    name: PathBuf,
    file: File,
    opened_metadata: CapMetadata,
}

impl OpenedLockLeaf {
    fn verify_namespace(&self) -> Result<(), LockSidecarReadFailure> {
        let named = self
            .directory
            .symlink_metadata(&self.name)
            .map_err(|error| stable_io_failure("lock_path_verify", &error))?;
        admit_regular_leaf(&named, "lock_path_verify")?;
        require_same_metadata_identity(&self.opened_metadata, &named, "lock_path_verify")?;
        require_same_metadata_observation(&self.opened_metadata, &named, "lock_path_verify")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenedLockReprobe {
    Held,
    Free,
}

fn reprobe_opened_lock(
    opened: &OpenedLockLeaf,
) -> Result<OpenedLockReprobe, LockSidecarReadFailure> {
    let outcome = match opened.file.try_lock_exclusive() {
        Ok(()) => OpenedLockReprobe::Free,
        Err(error) if is_lock_contended(&error) => OpenedLockReprobe::Held,
        Err(error) => return Err(stable_io_failure("lock_reprobe", &error)),
    };
    opened.verify_namespace()?;
    Ok(outcome)
}

fn open_lock_leaf_nofollow_with_hook(
    lock_path: &Path,
    create: bool,
    after_open: impl FnOnce(),
) -> Result<Option<OpenedLockLeaf>, LockSidecarReadFailure> {
    let (parent, name) = split_sidecar_path(lock_path)
        .ok_or_else(|| LockSidecarReadFailure::new("lock_path", "invalid_leaf"))?;
    let directory = match CapDir::open_ambient_dir(parent, cap_std::ambient_authority()) {
        Ok(directory) => directory,
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(stable_io_failure("lock_parent_open", &error)),
    };
    let named_before = match directory.symlink_metadata(name) {
        Ok(metadata) => {
            admit_regular_leaf(&metadata, "lock_path_before_open")?;
            Some(metadata)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(stable_io_failure("lock_path_before_open", &error)),
    };

    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    CapOpenOptionsPlatformExt::mode(&mut options, 0o600);
    let cap_file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(error) => {
            if let Ok(current) = directory.symlink_metadata(name) {
                if current.file_type().is_symlink() {
                    return Err(LockSidecarReadFailure::new("lock_open", "symlink"));
                }
                if !current.is_file() {
                    return Err(LockSidecarReadFailure::new(
                        "lock_open",
                        "not_regular_file",
                    ));
                }
            }
            return Err(stable_io_failure("lock_open", &error));
        }
    };
    let opened_metadata = cap_file
        .metadata()
        .map_err(|error| stable_io_failure("lock_handle_open", &error))?;
    admit_regular_leaf(&opened_metadata, "lock_handle_open")?;
    if let Some(named_before) = named_before.as_ref() {
        require_same_metadata_identity(named_before, &opened_metadata, "lock_open_identity")?;
        require_same_metadata_observation(named_before, &opened_metadata, "lock_open_identity")?;
    }

    after_open();

    let opened = OpenedLockLeaf {
        directory,
        name: name.to_path_buf(),
        file: cap_file.into_std(),
        opened_metadata,
    };
    opened.verify_namespace()?;
    Ok(Some(opened))
}

fn open_lock_leaf_nofollow(
    lock_path: &Path,
    create: bool,
) -> Result<Option<OpenedLockLeaf>, LockSidecarReadFailure> {
    open_lock_leaf_nofollow_with_hook(lock_path, create, || {})
}

fn lock_path_admission_error(failure: LockSidecarReadFailure) -> LockPathAdmissionError {
    match failure.kind {
        "symlink" => LockPathAdmissionError::Symlink,
        "not_regular_file" => LockPathAdmissionError::NotRegularFile,
        "namespace_identity_changed" | "metadata_changed" => {
            LockPathAdmissionError::ChangedDuringOpen
        }
        _ => LockPathAdmissionError::Unavailable,
    }
}

fn sidecar_write_error(failure: LockSidecarReadFailure) -> LockSidecarWriteError {
    match failure.kind {
        "symlink" => LockSidecarWriteError::Symlink,
        "not_regular_file" => LockSidecarWriteError::NotRegularFile,
        "oversized" => LockSidecarWriteError::Oversized,
        "namespace_identity_changed" | "metadata_changed" => {
            LockSidecarWriteError::ChangedDuringWrite
        }
        _ => LockSidecarWriteError::Unavailable,
    }
}

fn write_lock_sidecar_in_directory_atomically(
    directory: &CapDir,
    name: &Path,
    bytes: &[u8],
    max_bytes: usize,
    lock_for_authority: bool,
) -> Result<File, LockSidecarWriteError> {
    if bytes.len() > max_bytes {
        return Err(LockSidecarWriteError::Oversized);
    }
    match directory.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(LockSidecarWriteError::Symlink);
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(LockSidecarWriteError::NotRegularFile);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(LockSidecarWriteError::Unavailable),
    }

    for _ in 0..LOCK_SIDECAR_CREATE_ATTEMPTS {
        let nonce = LOCK_SIDECAR_WRITE_NONCE.fetch_add(1, AtomicOrdering::Relaxed);
        let epoch_nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let temp_name = format!(
            ".frankenterm-lock-sidecar-{}-{epoch_nanos}-{nonce}.tmp",
            std::process::id()
        );
        let mut options = CapOpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        CapOpenOptionsPlatformExt::mode(&mut options, 0o600);
        let mut temp_file = match directory.open_with(&temp_name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(LockSidecarWriteError::Unavailable),
        };
        let write_result = (|| {
            temp_file
                .write_all(bytes)
                .map_err(|_| LockSidecarWriteError::Unavailable)?;
            temp_file
                .sync_all()
                .map_err(|_| LockSidecarWriteError::Unavailable)?;
            let metadata_file = temp_file.into_std();
            if lock_for_authority {
                metadata_file
                    .try_lock_exclusive()
                    .map_err(|_| LockSidecarWriteError::Unavailable)?;
            }
            directory
                .rename(&temp_name, directory, name)
                .map_err(|_| LockSidecarWriteError::Unavailable)?;
            #[cfg(unix)]
            directory
                .open(".")
                .and_then(|directory_file| directory_file.sync_all())
                .map_err(|_| LockSidecarWriteError::Unavailable)?;

            let handle_metadata = CapMetadata::from_file(&metadata_file)
                .map_err(|_| LockSidecarWriteError::Unavailable)?;
            let named_metadata = directory
                .symlink_metadata(name)
                .map_err(|_| LockSidecarWriteError::Unavailable)?;
            admit_named_sidecar(&handle_metadata, max_bytes, "sidecar_write_handle")
                .map_err(sidecar_write_error)?;
            admit_named_sidecar(&named_metadata, max_bytes, "sidecar_write_path")
                .map_err(sidecar_write_error)?;
            require_same_metadata_identity(
                &handle_metadata,
                &named_metadata,
                "sidecar_write_identity",
            )
            .map_err(sidecar_write_error)?;
            require_same_metadata_observation(
                &handle_metadata,
                &named_metadata,
                "sidecar_write_identity",
            )
            .map_err(sidecar_write_error)?;
            Ok(metadata_file)
        })();

        // Never remove the temporary name after an error. Once an operation
        // has failed, another actor could replace that name before a
        // path-based cleanup, causing us to delete a file we did not create.
        // A rare private orphan is safer and can be diagnosed independently.
        return write_result;
    }
    Err(LockSidecarWriteError::Unavailable)
}

fn write_lock_sidecar_atomically(
    sidecar_path: &Path,
    bytes: &[u8],
    max_bytes: usize,
) -> Result<(), LockSidecarWriteError> {
    let (parent, name) =
        split_sidecar_path(sidecar_path).ok_or(LockSidecarWriteError::Unavailable)?;
    let directory = CapDir::open_ambient_dir(parent, cap_std::ambient_authority())
        .map_err(|_| LockSidecarWriteError::Unavailable)?;
    write_lock_sidecar_in_directory_atomically(&directory, name, bytes, max_bytes, false)?;
    Ok(())
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

    /// Handoff sidecar failed bounded filesystem/schema admission.
    #[error(transparent)]
    HandoffRead(#[from] WatcherHandoffReadError),

    /// The lock leaf failed no-follow regular-file admission.
    #[error(transparent)]
    LockPath(#[from] LockPathAdmissionError),

    /// A lock sidecar could not be persisted safely.
    #[error(transparent)]
    SidecarWrite(#[from] LockSidecarWriteError),

    /// The operating system could not provide entropy for a unique acquisition identity.
    #[error("secure entropy unavailable for watcher lock instance identity")]
    EntropyUnavailable,
}

/// Stable, content-free lock-leaf admission failures.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPathAdmissionError {
    /// The lock leaf was a symbolic link.
    #[error("watcher lock symlinks are not allowed")]
    Symlink,
    /// The lock leaf was not a regular file.
    #[error("watcher lock path is not a regular file")]
    NotRegularFile,
    /// The namespace identity changed while the lock leaf was opened.
    #[error("watcher lock path changed during admission")]
    ChangedDuringOpen,
    /// The lock leaf could not be opened or verified safely.
    #[error("watcher lock path is unavailable")]
    Unavailable,
}

/// Stable, content-free lock-sidecar persistence failures.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockSidecarWriteError {
    /// A pre-existing sidecar leaf was a symbolic link.
    #[error("watcher lock sidecar symlinks are not allowed")]
    Symlink,
    /// A pre-existing sidecar leaf was not a regular file.
    #[error("watcher lock sidecar is not a regular file")]
    NotRegularFile,
    /// Serialized sidecar bytes exceeded their schema-specific cap.
    #[error("watcher lock sidecar exceeds its safety limit")]
    Oversized,
    /// The sidecar namespace changed across atomic replacement.
    #[error("watcher lock sidecar changed during persistence")]
    ChangedDuringWrite,
    /// The sidecar could not be persisted or verified safely.
    #[error("watcher lock sidecar could not be persisted safely")]
    Unavailable,
}

/// Versioned protocol marker for zero-downtime watcher handoff records.
pub const WATCHER_HANDOFF_PROTOCOL_VERSION: u32 = 1;

/// Stable, content-free watcher handoff sidecar admission failures.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherHandoffReadError {
    /// The sidecar or its parent could not be read.
    #[error("watcher handoff sidecar is unavailable")]
    Unavailable,
    /// The sidecar leaf was a symbolic link.
    #[error("watcher handoff sidecar symlinks are not allowed")]
    Symlink,
    /// The sidecar leaf was not a regular file.
    #[error("watcher handoff sidecar is not a regular file")]
    NotRegularFile,
    /// The sidecar exceeded the schema-specific byte limit.
    #[error("watcher handoff sidecar exceeds its safety limit")]
    Oversized,
    /// Namespace or file observations changed during the bounded read.
    #[error("watcher handoff sidecar changed during admission")]
    ChangedDuringRead,
    /// The platform could not provide an identity/timestamp proof.
    #[error("watcher handoff sidecar could not be verified safely")]
    VerificationUnavailable,
    /// Bounded bytes did not decode as the concrete protocol schema.
    #[error("watcher handoff sidecar has an invalid schema")]
    InvalidSchema,
}

/// Validation failures for watcher handoff sidecar records.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum WatcherHandoffError {
    /// The sidecar came from an unsupported protocol version.
    #[error("unsupported protocol_version {got}, expected {expected}")]
    UnsupportedProtocolVersion { got: u32, expected: u32 },

    /// Generation zero is reserved for the pre-handoff single-watcher state.
    #[error("handoff generation must be greater than zero")]
    ZeroGeneration,

    /// PID zero has process-group semantics and cannot identify a watcher.
    #[error("handoff predecessor pid must be greater than zero")]
    ZeroPredecessorPid,

    /// PID zero has process-group semantics and cannot identify a watcher.
    #[error("handoff successor pid must be greater than zero")]
    ZeroSuccessorPid,

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
/// drain-and-takeover runtime wiring. The current record is deliberately inert:
/// its PID fields are diagnostic and it does not yet carry exact acquisition
/// identities, so no watcher action may be authorized from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatcherHandoffRecord {
    /// Protocol schema version for forward-compatible parsing.
    pub protocol_version: u32,
    /// Monotonic handoff generation. Generation 0 means no handoff.
    pub generation: u64,
    /// Diagnostic PID of the watcher expected to drain; never control authority.
    pub predecessor_pid: u32,
    /// Diagnostic PID of the standby successor, if known; never control authority.
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

    /// Validate the sidecar's finite schema and internal consistency.
    ///
    /// This does not authorize any process or watcher action. PID fields are
    /// diagnostic and recyclable, and the current handoff sidecar is not bound
    /// to an exact acquired watcher instance. The record remains inert until a
    /// future protocol revision adds that authority binding.
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
        if self.predecessor_pid == 0 {
            return Err(WatcherHandoffError::ZeroPredecessorPid);
        }
        if self.successor_pid == Some(0) {
            return Err(WatcherHandoffError::ZeroSuccessorPid);
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockMetadata {
    /// Diagnostic process ID of the lock holder; never a durable control handle.
    pub pid: u32,
    /// Unix timestamp when the lock was acquired.
    pub started_at: u64,
    /// Human-readable start time.
    pub started_at_human: String,
    /// FrankenTerm version that acquired the lock.
    pub wa_version: String,
    /// Collision-resistant identity generated independently for this acquisition.
    pub instance_id: String,
}

impl LockMetadata {
    /// Create new metadata for the current process.
    fn new() -> Result<Self, LockError> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut instance_bytes = [0_u8; LOCK_INSTANCE_ID_BYTES];
        let mut rng = SysRng;
        rng.try_fill_bytes(&mut instance_bytes)
            .map_err(|_| LockError::EntropyUnavailable)?;

        Ok(Self::from_instance_bytes(now, instance_bytes))
    }

    fn from_instance_bytes(now: u64, instance_bytes: [u8; LOCK_INSTANCE_ID_BYTES]) -> Self {
        let instance_id = u128::from_be_bytes(instance_bytes);
        Self {
            pid: std::process::id(),
            started_at: now,
            started_at_human: chrono_lite_format(now),
            wa_version: crate::VERSION.to_string(),
            instance_id: format!("{instance_id:032x}"),
        }
    }

    fn is_admissible(&self) -> bool {
        self.pid != 0
            && self.started_at_human == chrono_lite_format(self.started_at)
            && !self.wa_version.is_empty()
            && self.wa_version.len() <= MAX_LOCK_METADATA_VERSION_BYTES
            && !self.wa_version.chars().any(char::is_control)
            && self.instance_id.len() == LOCK_INSTANCE_ID_HEX_LEN
            && self
                .instance_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    }
}

/// Truthful result of probing the watcher lock.
///
/// A held lock with unavailable metadata remains distinct from a free lock,
/// and inability to probe the lock itself remains distinct from both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockStatus {
    /// The lock was acquired by the probe and is therefore currently free.
    Free,
    /// The lock is held and the holder-locked metadata inode passed admission.
    /// This is a point-in-time observation; `pid` is diagnostic data, not a
    /// durable process handle. Cooperative control must bind `instance_id`.
    HeldKnown(LockMetadata),
    /// The lock is held, but its metadata is missing, unsafe, or invalid.
    HeldUnknown,
    /// The lock file could not be opened or probed reliably.
    ProbeUnavailable,
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
    // Field order is authority order: the metadata inode lock is released
    // before the main watcher lock, so a probe can observe HeldUnknown during
    // teardown but can never attribute a successor to predecessor metadata.
    _metadata_file: File,
    _lock_file: File,
    _lock_directory: CapDir,
    _lock_name: PathBuf,
    lock_path: PathBuf,
    meta_path: PathBuf,
    metadata: LockMetadata,
}

impl WatcherLock {
    /// Attempt to acquire the single-instance lock.
    ///
    /// Returns `Ok(WatcherLock)` if the lock was acquired successfully.
    /// Returns `Err(LockError::AlreadyRunning)` if another instance holds the lock.
    pub fn acquire(lock_path: &Path) -> Result<Self, LockError> {
        Self::acquire_with_hook(lock_path, || {})
    }

    fn acquire_with_hook(
        lock_path: &Path,
        after_metadata_write: impl FnOnce(),
    ) -> Result<Self, LockError> {
        // Ensure parent directory exists
        if let Some(parent) = lock_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let opened = match open_lock_leaf_nofollow(lock_path, true) {
            Ok(Some(opened)) => opened,
            Ok(None) => return Err(LockPathAdmissionError::Unavailable.into()),
            Err(failure) => {
                tracing::warn!(
                    target: "frankenterm::lock",
                    event = "ft-interactive-systems-performance-4tenz.53",
                    phase = failure.phase,
                    kind = failure.kind,
                    "watcher lock path failed no-follow admission"
                );
                return Err(lock_path_admission_error(failure).into());
            }
        };

        // Try to acquire exclusive lock (non-blocking)
        match opened.file.try_lock_exclusive() {
            Ok(()) => {
                opened
                    .verify_namespace()
                    .map_err(lock_path_admission_error)?;
                let meta_path = metadata_path(lock_path);
                let meta_name = metadata_path(&opened.name);
                let metadata = LockMetadata::new()?;
                let json = serde_json::to_vec_pretty(&metadata)?;
                let metadata_file = write_lock_sidecar_in_directory_atomically(
                    &opened.directory,
                    &meta_name,
                    &json,
                    MAX_LOCK_METADATA_BYTES,
                    true,
                )?;

                after_metadata_write();
                opened
                    .verify_namespace()
                    .map_err(lock_path_admission_error)?;

                let OpenedLockLeaf {
                    directory: lock_directory,
                    name: lock_name,
                    file: lock_file,
                    ..
                } = opened;
                let lock = Self {
                    _metadata_file: metadata_file,
                    _lock_file: lock_file,
                    _lock_directory: lock_directory,
                    _lock_name: lock_name,
                    lock_path: lock_path.to_path_buf(),
                    meta_path,
                    metadata,
                };
                tracing::debug!(
                    target: "frankenterm::lock",
                    event = "ft-interactive-systems-performance-4tenz.53",
                    phase = "acquire",
                    kind = "success",
                    pid = lock.metadata.pid,
                    "acquired watcher lock"
                );
                Ok(lock)
            }
            Err(e) if is_lock_contended(&e) => {
                opened
                    .verify_namespace()
                    .map_err(lock_path_admission_error)?;
                Err(read_existing_lock_error_for_opened(&opened))
            }
            Err(e) => Err(LockError::Io(e)),
        }
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

    /// Exact metadata published and authority-bound by this acquisition.
    #[must_use]
    pub fn metadata(&self) -> &LockMetadata {
        &self.metadata
    }
}

impl Drop for WatcherLock {
    fn drop(&mut self) {
        // The metadata sidecar is deliberately retained. A check followed by
        // path-based removal has an unavoidable replacement race in safe Rust:
        // another actor can swap the name between verification and unlink.
        // The retained sidecar authorizes an identity only while its exact
        // admitted inode is advisory-locked. Field drop order releases that
        // authority before the main watcher lock; the next successful acquire
        // atomically publishes and locks its own metadata inode.
        tracing::debug!(
            target: "frankenterm::lock",
            event = "ft-interactive-systems-performance-4tenz.53",
            phase = "release",
            kind = "metadata_retained",
            "releasing watcher lock and retaining diagnostic metadata"
        );
        // Note: The actual file lock is released when _lock_file is dropped
    }
}

/// Compute the metadata sidecar path for a given lock path.
fn metadata_path(lock_path: &Path) -> PathBuf {
    let mut meta_path = lock_path.to_path_buf();
    let mut file_name = lock_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("lock"))
        .to_os_string();
    file_name.push(".meta.json");
    meta_path.set_file_name(file_name);
    meta_path
}

/// Compute the zero-downtime handoff sidecar path for a given lock path.
#[must_use]
pub fn watcher_handoff_path(lock_path: &Path) -> PathBuf {
    let mut path = lock_path.to_path_buf();
    let mut file_name = lock_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("lock"))
        .to_os_string();
    file_name.push(".handoff.json");
    path.set_file_name(file_name);
    path
}

/// Persist a watcher handoff record next to the lock file.
pub fn write_watcher_handoff_record(
    lock_path: &Path,
    record: &WatcherHandoffRecord,
) -> Result<PathBuf, LockError> {
    record.validate()?;
    let path = watcher_handoff_path(lock_path);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(record)?;
    write_lock_sidecar_atomically(&path, &json, MAX_WATCHER_HANDOFF_BYTES)?;
    Ok(path)
}

fn watcher_handoff_read_error(failure: LockSidecarReadFailure) -> WatcherHandoffReadError {
    match failure.kind {
        "symlink" => WatcherHandoffReadError::Symlink,
        "not_regular_file" => WatcherHandoffReadError::NotRegularFile,
        "oversized" => WatcherHandoffReadError::Oversized,
        "namespace_identity_changed" | "metadata_changed" => {
            WatcherHandoffReadError::ChangedDuringRead
        }
        "identity_unavailable" | "timestamp_unavailable" => {
            WatcherHandoffReadError::VerificationUnavailable
        }
        "invalid_schema" => WatcherHandoffReadError::InvalidSchema,
        _ => WatcherHandoffReadError::Unavailable,
    }
}

fn read_watcher_handoff_record_with_hook(
    lock_path: &Path,
    after_open: impl FnOnce(),
) -> Result<Option<WatcherHandoffRecord>, LockError> {
    let path = watcher_handoff_path(lock_path);
    let raw = match read_lock_sidecar_bytes_admitted_with_hook(
        &path,
        MAX_WATCHER_HANDOFF_BYTES,
        after_open,
    ) {
        Ok(raw) => raw,
        Err(failure)
            if failure.kind == "not_found"
                && matches!(failure.phase, "parent_open" | "path_before_open") =>
        {
            return Ok(None);
        }
        Err(failure) => {
            report_watcher_handoff_read_failure(failure);
            return Err(watcher_handoff_read_error(failure).into());
        }
    };
    let record = match serde_json::from_slice::<WatcherHandoffRecord>(&raw) {
        Ok(record) => record,
        Err(_) => {
            let failure = LockSidecarReadFailure::with_size(
                "parse",
                "invalid_schema",
                u64::try_from(raw.len()).unwrap_or(MAX_WATCHER_HANDOFF_BYTES as u64 + 1),
            );
            report_watcher_handoff_read_failure(failure);
            return Err(WatcherHandoffReadError::InvalidSchema.into());
        }
    };
    if let Err(error) = record.validate() {
        report_watcher_handoff_read_failure(LockSidecarReadFailure::with_size(
            "validate",
            "invalid_protocol",
            u64::try_from(raw.len()).unwrap_or(MAX_WATCHER_HANDOFF_BYTES as u64 + 1),
        ));
        return Err(error.into());
    }
    Ok(Some(record))
}

/// Read a watcher handoff record if the sidecar exists.
///
/// The sidecar is opened without following links and admitted through the
/// same bounded, race-aware regular-file checks as lock-holder metadata.
pub fn read_watcher_handoff_record(
    lock_path: &Path,
) -> Result<Option<WatcherHandoffRecord>, LockError> {
    read_watcher_handoff_record_with_hook(lock_path, || {})
}

/// Read metadata from an existing lock to provide a helpful error message.
#[cfg(test)]
fn read_existing_lock_error(lock_path: &Path) -> LockError {
    let meta_path = metadata_path(lock_path);
    match read_lock_metadata(&meta_path) {
        Ok(meta) => LockError::AlreadyRunning {
            pid: meta.pid,
            started_at: meta.started_at_human,
        },
        Err(_) => LockError::AlreadyRunningNoMeta,
    }
}

fn read_existing_lock_error_for_opened(opened: &OpenedLockLeaf) -> LockError {
    match probe_held_lock_metadata_with_hooks(opened, || {}, || {}) {
        LockStatus::HeldKnown(meta) => LockError::AlreadyRunning {
            pid: meta.pid,
            started_at: meta.started_at_human,
        },
        LockStatus::Free | LockStatus::HeldUnknown | LockStatus::ProbeUnavailable => {
            LockError::AlreadyRunningNoMeta
        }
    }
}

fn report_lock_reprobe_failure(failure: LockSidecarReadFailure) {
    tracing::warn!(
        target: "frankenterm::lock",
        event = "ft-interactive-systems-performance-4tenz.53",
        phase = failure.phase,
        kind = failure.kind,
        "watcher lock authority changed during metadata attribution"
    );
}

fn reconcile_metadata_failure_with_main_lock(
    opened: &OpenedLockLeaf,
    failure: LockSidecarReadFailure,
) -> LockStatus {
    report_lock_metadata_read_failure(failure);
    match reprobe_opened_lock(opened) {
        Ok(OpenedLockReprobe::Held) => LockStatus::HeldUnknown,
        Ok(OpenedLockReprobe::Free) => LockStatus::Free,
        Err(reprobe_failure) => {
            report_lock_reprobe_failure(reprobe_failure);
            LockStatus::ProbeUnavailable
        }
    }
}

fn probe_held_lock_metadata_with_hooks(
    opened: &OpenedLockLeaf,
    after_metadata_decode: impl FnOnce(),
    after_main_lock_reprobe: impl FnOnce(),
) -> LockStatus {
    let meta_name = metadata_path(&opened.name);
    let (metadata, admitted) =
        match read_lock_metadata_in_directory_retained(&opened.directory, &meta_name) {
            Ok(result) => result,
            Err(failure) => return reconcile_metadata_failure_with_main_lock(opened, failure),
        };

    // Keep the exact admitted metadata handle alive across schema decoding and
    // this final main-lock probe. A holder that exits or hands off between the
    // first contention observation and metadata attribution must not leave a
    // stale numeric identity authorized by otherwise valid bytes.
    after_metadata_decode();
    match reprobe_opened_lock(opened) {
        Ok(OpenedLockReprobe::Held) => {}
        Ok(OpenedLockReprobe::Free) => return LockStatus::Free,
        Err(failure) => {
            report_lock_reprobe_failure(failure);
            return LockStatus::ProbeUnavailable;
        }
    }

    // The hook is a deterministic transition seam for tests. Production uses
    // a no-op, then revalidates both the exact sidecar inode and its holder lock
    // after the main lock has been proven contended a second time.
    after_main_lock_reprobe();
    if let Err(failure) = verify_retained_lock_metadata_authority(
        &opened.directory,
        &meta_name,
        &admitted,
    ) {
        return reconcile_metadata_failure_with_main_lock(opened, failure);
    }

    LockStatus::HeldKnown(metadata)
}

/// Check if a watcher is currently running with a brief nonblocking lock probe.
///
/// This never treats rejected holder metadata as evidence that the lock is
/// free. Callers must handle every [`LockStatus`] variant explicitly. A
/// [`LockStatus::HeldKnown`] return does not authorize a later numeric-PID
/// signal because the holder can exit and the operating system can recycle
/// that PID immediately after this point-in-time probe.
#[must_use]
pub fn check_running(lock_path: &Path) -> LockStatus {
    check_running_with_hooks(lock_path, || {}, || {})
}

fn check_running_with_hooks(
    lock_path: &Path,
    after_metadata_decode: impl FnOnce(),
    after_main_lock_reprobe: impl FnOnce(),
) -> LockStatus {
    let opened = match open_lock_leaf_nofollow(lock_path, false) {
        Ok(Some(opened)) => opened,
        Ok(None) => return LockStatus::Free,
        Err(failure) => {
            tracing::warn!(
                target: "frankenterm::lock",
                event = "ft-interactive-systems-performance-4tenz.53",
                phase = failure.phase,
                kind = failure.kind,
                "watcher lock status probe was unavailable"
            );
            return LockStatus::ProbeUnavailable;
        }
    };

    // Try to acquire lock - if it fails, something is holding it
    match opened.file.try_lock_exclusive() {
        Ok(()) => {
            match opened.verify_namespace() {
                Ok(()) => LockStatus::Free,
                Err(failure) => {
                    tracing::warn!(
                        target: "frankenterm::lock",
                        event = "ft-interactive-systems-performance-4tenz.53",
                        phase = failure.phase,
                        kind = failure.kind,
                        "watcher lock status probe changed during verification"
                    );
                    LockStatus::ProbeUnavailable
                }
            }
        }
        Err(e) if is_lock_contended(&e) => {
            if let Err(failure) = opened.verify_namespace() {
                tracing::warn!(
                    target: "frankenterm::lock",
                    event = "ft-interactive-systems-performance-4tenz.53",
                    phase = failure.phase,
                    kind = failure.kind,
                    "held watcher lock changed during verification"
                );
                return LockStatus::ProbeUnavailable;
            }
            probe_held_lock_metadata_with_hooks(
                &opened,
                after_metadata_decode,
                after_main_lock_reprobe,
            )
        }
        Err(error) => {
            tracing::warn!(
                target: "frankenterm::lock",
                event = "ft-interactive-systems-performance-4tenz.53",
                phase = "lock_probe",
                kind = stable_io_failure("lock_probe", &error).kind,
                "watcher lock status probe was unavailable"
            );
            LockStatus::ProbeUnavailable
        }
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

        // Drop releases the authoritative lock but deliberately retains the
        // diagnostic sidecar to avoid an unsafe check-then-unlink race.
        drop(lock);
        assert!(meta_path.exists());
        assert_eq!(check_running(&lock_path), LockStatus::Free);
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
        assert_eq!(check_running(&lock_path), LockStatus::Free);

        let _lock = WatcherLock::acquire(&lock_path).unwrap();

        // Now lock is held
        match check_running(&lock_path) {
            LockStatus::HeldKnown(metadata) => {
                assert_eq!(metadata.pid, std::process::id());
            }
            status => panic!("expected known held lock, got {status:?}"),
        }
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
    fn lock_error_display_entropy_unavailable_is_content_free() {
        assert_eq!(
            LockError::EntropyUnavailable.to_string(),
            "secure entropy unavailable for watcher lock instance identity"
        );
    }

    #[test]
    fn lock_metadata_new_has_valid_fields() {
        let meta = LockMetadata::new().unwrap();
        assert_eq!(meta.pid, std::process::id());
        assert!(meta.started_at > 0);
        assert!(meta.started_at_human.starts_with("unix:"));
        assert!(!meta.wa_version.is_empty());
        assert_eq!(meta.instance_id.len(), LOCK_INSTANCE_ID_HEX_LEN);
        assert!(meta.is_admissible());
    }

    #[test]
    fn lock_metadata_instance_byte_encoding_is_canonical_and_injective_for_fixtures() {
        let first = LockMetadata::from_instance_bytes(1, [0_u8; LOCK_INSTANCE_ID_BYTES]);
        let second = LockMetadata::from_instance_bytes(1, [0xff_u8; LOCK_INSTANCE_ID_BYTES]);

        assert_ne!(first.instance_id, second.instance_id);
        assert_eq!(first.instance_id, "00000000000000000000000000000000");
        assert_eq!(second.instance_id, "ffffffffffffffffffffffffffffffff");
        for instance_id in [&first.instance_id, &second.instance_id] {
            assert_eq!(instance_id.len(), LOCK_INSTANCE_ID_HEX_LEN);
            assert!(
                instance_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            );
        }
    }

    #[test]
    fn lock_metadata_serde_roundtrip() {
        let meta = LockMetadata {
            pid: 999,
            started_at: 1_700_000_000,
            started_at_human: "unix:1700000000".to_string(),
            wa_version: "0.1.0".to_string(),
            instance_id: "0123456789abcdef0123456789abcdef".to_string(),
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

        let record = WatcherHandoffRecord::drain_requested(1, 0, 200);
        assert!(matches!(
            record.validate(),
            Err(WatcherHandoffError::ZeroPredecessorPid)
        ));

        let record = WatcherHandoffRecord::drain_requested(1, 100, 0);
        assert!(matches!(
            record.validate(),
            Err(WatcherHandoffError::ZeroSuccessorPid)
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
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_watcher_handoff_admission_failure_count_for_test();
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
        assert_eq!(watcher_handoff_admission_failure_count(), 1);
    }

    #[test]
    fn watcher_handoff_read_rejects_unknown_schema_fields() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_watcher_handoff_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        let handoff_path = watcher_handoff_path(&lock_path);
        let mut value = serde_json::to_value(WatcherHandoffRecord::drain_requested(
            7, 100, 200,
        ))
        .unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unreviewed_field".to_string(), serde_json::json!(true));
        fs::write(&handoff_path, serde_json::to_vec(&value).unwrap()).unwrap();

        assert!(matches!(
            read_watcher_handoff_record(&lock_path),
            Err(LockError::HandoffRead(
                WatcherHandoffReadError::InvalidSchema
            ))
        ));
        assert_eq!(watcher_handoff_admission_failure_count(), 1);
    }

    #[test]
    fn watcher_handoff_exact_cap_is_admitted() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_watcher_handoff_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        let path = watcher_handoff_path(&lock_path);
        let record = WatcherHandoffRecord::drain_requested(7, 100, 200);
        let mut raw = serde_json::to_vec(&record).unwrap();
        assert!(raw.len() <= MAX_WATCHER_HANDOFF_BYTES);
        raw.resize(MAX_WATCHER_HANDOFF_BYTES, b' ');
        fs::write(&path, raw).unwrap();

        assert_eq!(
            read_watcher_handoff_record(&lock_path).unwrap(),
            Some(record)
        );
        assert_eq!(watcher_handoff_admission_failure_count(), 0);
    }

    #[test]
    fn watcher_handoff_one_byte_over_cap_is_rejected() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_watcher_handoff_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        let path = watcher_handoff_path(&lock_path);
        let record = WatcherHandoffRecord::drain_requested(7, 100, 200);
        let mut raw = serde_json::to_vec(&record).unwrap();
        raw.resize(MAX_WATCHER_HANDOFF_BYTES + 1, b' ');
        fs::write(&path, raw).unwrap();

        assert!(matches!(
            read_watcher_handoff_record(&lock_path),
            Err(LockError::HandoffRead(
                WatcherHandoffReadError::Oversized
            ))
        ));
        assert_eq!(watcher_handoff_admission_failure_count(), 1);
    }

    #[test]
    fn watcher_handoff_malformed_failure_is_content_free() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_watcher_handoff_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("private-handoff-path-canary.lock");
        let path = watcher_handoff_path(&lock_path);
        let content_canary = "AKIA-HANDOFF-CONTENT-CANARY";
        fs::write(&path, format!(r#"{{"generation":"{content_canary}""#)).unwrap();

        let error = read_watcher_handoff_record(&lock_path).unwrap_err();
        let rendered = format!("{error:?} {}", error);
        assert!(matches!(
            error,
            LockError::HandoffRead(WatcherHandoffReadError::InvalidSchema)
        ));
        assert!(!rendered.contains("private-handoff-path-canary"));
        assert!(!rendered.contains(content_canary));
        assert_eq!(watcher_handoff_admission_failure_count(), 1);
    }

    #[test]
    fn watcher_handoff_directory_is_rejected_as_non_regular() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_watcher_handoff_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        fs::create_dir(watcher_handoff_path(&lock_path)).unwrap();

        assert!(matches!(
            read_watcher_handoff_record(&lock_path),
            Err(LockError::HandoffRead(
                WatcherHandoffReadError::NotRegularFile
            ))
        ));
        assert_eq!(watcher_handoff_admission_failure_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn watcher_handoff_symlink_is_rejected_without_following() {
        use std::os::unix::fs::symlink;

        let _counter_guard = lock_metadata_counter_test_lock();
        reset_watcher_handoff_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        let target = tmp.path().join("target.handoff.json");
        let link = watcher_handoff_path(&lock_path);
        fs::write(
            &target,
            serde_json::to_vec(&WatcherHandoffRecord::drain_requested(7, 100, 200)).unwrap(),
        )
        .unwrap();
        symlink(&target, &link).unwrap();

        assert!(matches!(
            read_watcher_handoff_record(&lock_path),
            Err(LockError::HandoffRead(WatcherHandoffReadError::Symlink))
        ));
        assert_eq!(watcher_handoff_admission_failure_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn watcher_handoff_namespace_replacement_after_open_is_rejected() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_watcher_handoff_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        let path = watcher_handoff_path(&lock_path);
        let moved = tmp.path().join("watch.original.handoff.json");
        let replacement = tmp.path().join("watch.replacement.handoff.json");
        let raw = serde_json::to_vec(&WatcherHandoffRecord::drain_requested(7, 100, 200)).unwrap();
        fs::write(&path, &raw).unwrap();
        fs::write(&replacement, &raw).unwrap();

        let error = read_watcher_handoff_record_with_hook(&lock_path, || {
            fs::rename(&path, &moved).unwrap();
            fs::rename(&replacement, &path).unwrap();
        })
        .unwrap_err();
        assert!(matches!(
            error,
            LockError::HandoffRead(WatcherHandoffReadError::ChangedDuringRead)
        ));
        assert_eq!(watcher_handoff_admission_failure_count(), 1);
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
            instance_id: "11111111111111111111111111111111".to_string(),
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
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");
        let meta_path = metadata_path(&lock_path);

        fs::write(&meta_path, "not valid json").unwrap();

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn read_existing_lock_error_no_meta_file() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
        assert_eq!(lock_metadata_admission_failure_count(), 1);
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
    fn check_running_no_file_reports_free() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("nonexistent.lock");
        assert_eq!(check_running(&lock_path), LockStatus::Free);
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

    #[cfg(unix)]
    #[test]
    fn lock_leaf_symlink_is_never_followed_or_reported_free() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("lock-target");
        let lock_path = tmp.path().join("watcher.lock");
        fs::write(&target, "lock-target-canary").unwrap();
        symlink(&target, &lock_path).unwrap();

        assert!(matches!(
            WatcherLock::acquire(&lock_path),
            Err(LockError::LockPath(LockPathAdmissionError::Symlink))
        ));
        assert_eq!(check_running(&lock_path), LockStatus::ProbeUnavailable);
        assert_eq!(fs::read_to_string(&target).unwrap(), "lock-target-canary");
    }

    #[test]
    fn lock_leaf_directory_is_probe_unavailable() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        fs::create_dir(&lock_path).unwrap();

        assert!(matches!(
            WatcherLock::acquire(&lock_path),
            Err(LockError::LockPath(
                LockPathAdmissionError::NotRegularFile
            ))
        ));
        assert_eq!(check_running(&lock_path), LockStatus::ProbeUnavailable);
    }

    #[cfg(unix)]
    #[test]
    fn lock_leaf_replacement_during_open_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let moved = tmp.path().join("watcher.original.lock");
        let replacement = tmp.path().join("watcher.replacement.lock");
        fs::write(&lock_path, "").unwrap();
        fs::write(&replacement, "").unwrap();

        let failure = open_lock_leaf_nofollow_with_hook(&lock_path, false, || {
            fs::rename(&lock_path, &moved).unwrap();
            fs::rename(&replacement, &lock_path).unwrap();
        })
        .err()
        .expect("namespace replacement must fail admission");
        assert_eq!(failure.phase, "lock_path_verify");
        assert_eq!(failure.kind, "namespace_identity_changed");
    }

    #[cfg(unix)]
    #[test]
    fn lock_metadata_write_rejects_preplanted_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let meta_path = metadata_path(&lock_path);
        let target = tmp.path().join("metadata-target");
        fs::write(&target, "metadata-target-canary").unwrap();
        symlink(&target, &meta_path).unwrap();

        assert!(matches!(
            WatcherLock::acquire(&lock_path),
            Err(LockError::SidecarWrite(LockSidecarWriteError::Symlink))
        ));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "metadata-target-canary"
        );
        assert_eq!(check_running(&lock_path), LockStatus::Free);
    }

    #[cfg(unix)]
    #[test]
    fn handoff_write_rejects_preplanted_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let handoff_path = watcher_handoff_path(&lock_path);
        let target = tmp.path().join("handoff-target");
        fs::write(&target, "handoff-target-canary").unwrap();
        symlink(&target, &handoff_path).unwrap();

        assert!(matches!(
            write_watcher_handoff_record(
                &lock_path,
                &WatcherHandoffRecord::drain_requested(7, 100, 200),
            ),
            Err(LockError::SidecarWrite(LockSidecarWriteError::Symlink))
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "handoff-target-canary");
    }

    #[cfg(unix)]
    #[test]
    fn lock_drop_retains_replaced_metadata_leaf() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let lock = WatcherLock::acquire(&lock_path).unwrap();
        let meta_path = lock.meta_path().to_path_buf();
        let original = tmp.path().join("watcher.original.meta.json");
        fs::rename(&meta_path, &original).unwrap();
        let replacement = LockMetadata {
            pid: 91_919,
            started_at: 1_600_000_001,
            started_at_human: "unix:1600000001".to_string(),
            wa_version: "replacement-metadata-canary".to_string(),
            instance_id: "22222222222222222222222222222222".to_string(),
        };
        let replacement_bytes = serde_json::to_vec(&replacement).unwrap();
        fs::write(&meta_path, &replacement_bytes).unwrap();

        assert_eq!(check_running(&lock_path), LockStatus::HeldUnknown);
        assert_eq!(lock_metadata_admission_failure_count(), 1);

        drop(lock);

        assert_eq!(fs::read(&meta_path).unwrap(), replacement_bytes);
        assert!(original.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn acquire_rejects_lock_replacement_after_metadata_publication() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let moved = tmp.path().join("watcher.original.lock");
        let replacement = tmp.path().join("watcher.replacement.lock");
        fs::write(&lock_path, "").unwrap();
        fs::write(&replacement, "").unwrap();

        let error = WatcherLock::acquire_with_hook(&lock_path, || {
            fs::rename(&lock_path, &moved).unwrap();
            fs::rename(&replacement, &lock_path).unwrap();
        })
        .unwrap_err();

        assert!(matches!(
            error,
            LockError::LockPath(LockPathAdmissionError::ChangedDuringOpen)
        ));
        assert_eq!(check_running(&lock_path), LockStatus::Free);
        assert!(metadata_path(&lock_path).is_file());
    }

    #[test]
    fn retained_stale_metadata_is_harmless_and_atomically_replaced() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let first = WatcherLock::acquire(&lock_path).unwrap();
        let meta_path = first.meta_path().to_path_buf();
        drop(first);

        assert_eq!(check_running(&lock_path), LockStatus::Free);
        fs::write(&meta_path, "stale-metadata-canary").unwrap();
        assert_eq!(check_running(&lock_path), LockStatus::Free);

        let second = WatcherLock::acquire(&lock_path).unwrap();
        let raw = fs::read(&meta_path).unwrap();
        let metadata = serde_json::from_slice::<LockMetadata>(&raw).unwrap();
        assert!(metadata.is_admissible());
        assert_ne!(raw.as_slice(), b"stale-metadata-canary");
        drop(second);
        assert_eq!(check_running(&lock_path), LockStatus::Free);
    }

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
        let meta = LockMetadata::new().unwrap();
        let dbg = format!("{meta:?}");
        assert!(dbg.contains("pid"));
        assert!(dbg.contains("started_at"));
        assert!(dbg.contains("wa_version"));
        assert!(dbg.contains("instance_id"));
    }

    #[test]
    fn lock_metadata_clone() {
        let meta = LockMetadata {
            pid: 123,
            started_at: 456,
            started_at_human: "unix:456".to_string(),
            wa_version: "1.0".to_string(),
            instance_id: "33333333333333333333333333333333".to_string(),
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
    fn metadata_retained_after_drop_without_affecting_lock_truth() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("cleanup.lock");
        let meta_path;
        {
            let lock = WatcherLock::acquire(&lock_path).unwrap();
            meta_path = lock.meta_path().to_path_buf();
            assert!(meta_path.exists());
        }
        // Metadata is retained because path-based deletion cannot be made
        // replacement-safe. It cannot authorize a holder after its advisory
        // binding is released.
        assert!(meta_path.exists());
        assert_eq!(check_running(&lock_path), LockStatus::Free);
        // Lock file itself remains (it's just a file, the OS lock is released)
        assert!(lock_path.exists());
    }

    #[test]
    fn check_running_after_release_reports_free() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("released.lock");

        let lock = WatcherLock::acquire(&lock_path).unwrap();
        drop(lock);

        assert_eq!(check_running(&lock_path), LockStatus::Free);
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
            instance_id: "44444444444444444444444444444444".to_string(),
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
            instance_id: "55555555555555555555555555555555".to_string(),
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
            instance_id: "66666666666666666666666666666666".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, 0);
        assert!(!back.is_admissible());
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
    fn watcher_lock_metadata_accessor_matches_published_record() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("metadata-accessor.lock");
        let lock = WatcherLock::acquire(&lock_path).unwrap();
        let published: LockMetadata =
            serde_json::from_slice(&fs::read(lock.meta_path()).unwrap()).unwrap();

        assert_eq!(lock.metadata(), &published);
        assert!(published.is_admissible());
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
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("empty.lock");
        let meta_path = metadata_path(&lock_path);

        fs::write(&meta_path, "").unwrap();

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn read_existing_lock_error_partial_json() {
        let _counter_guard = lock_metadata_counter_test_lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("partial.lock");
        let meta_path = metadata_path(&lock_path);

        fs::write(&meta_path, r#"{"pid": 42"#).unwrap();

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
        assert_eq!(lock_metadata_admission_failure_count(), 1);
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
            instance_id: "77777777777777777777777777777777".to_string(),
        };
        fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        assert_eq!(check_running(&lock_path), LockStatus::Free);
    }

    #[test]
    fn lock_error_display_contains_io_message() {
        let err = LockError::Io(io::Error::other("custom error message"));
        assert!(err.to_string().contains("custom error message"));
    }

    #[test]
    fn lock_telemetry_source_has_no_raw_path_or_error_fields() {
        let source = include_str!("lock.rs");
        for field in ["lock_path", "meta_path", "error"] {
            let forbidden = format!("{field} = {}", '%');
            assert!(
                !source.contains(&forbidden),
                "lock telemetry must not contain raw field pattern {forbidden}"
            );
        }
    }

    #[test]
    fn lock_sidecar_code_never_unlinks_a_raceable_name() {
        let source = include_str!("lock.rs");
        let forbidden = ["remove_", "file"].concat();
        assert!(
            !source.contains(&forbidden),
            "lock sidecars must be retained instead of unlinked by path"
        );
    }
}

// Serialize tests that touch the process-global metadata-admission counter so
// concurrent test threads
// don't race on reset/observe pairs.
#[cfg(test)]
mod metadata_admission_tests {
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
            instance_id: "88888888888888888888888888888888".to_string(),
        }
    }

    fn padded_well_formed_metadata(target_len: usize) -> Vec<u8> {
        let mut raw = serde_json::to_vec(&well_formed_metadata()).unwrap();
        assert!(raw.len() <= target_len);
        raw.resize(target_len, b' ');
        raw
    }

    #[test]
    fn well_formed_metadata_does_not_bump() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.lock.meta");
        let raw = serde_json::to_string(&well_formed_metadata()).unwrap();
        fs::write(&path, raw).unwrap();
        let meta = read_lock_metadata(&path);
        assert_eq!(meta.unwrap().pid, 1234);
        assert_eq!(lock_metadata_admission_failure_count(), 0);
    }

    #[test]
    fn noncanonical_instance_ids_fail_closed() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.lock.meta");
        let invalid_ids = [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "g123456789abcdef0123456789abcdef",
        ];

        for invalid_id in invalid_ids {
            let mut metadata = well_formed_metadata();
            metadata.instance_id = invalid_id.to_string();
            fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
            let failure = read_lock_metadata(&path).unwrap_err();
            assert_eq!(failure.phase, "validate");
            assert_eq!(failure.kind, "invalid_schema");
        }
        assert_eq!(
            lock_metadata_admission_failure_count(),
            u64::try_from(invalid_ids.len()).unwrap()
        );
    }

    #[test]
    fn exact_cap_metadata_is_admitted() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("exact-cap.meta");
        let raw = padded_well_formed_metadata(MAX_LOCK_METADATA_BYTES);
        assert_eq!(raw.len(), MAX_LOCK_METADATA_BYTES);
        fs::write(&path, raw).unwrap();

        assert_eq!(read_lock_metadata(&path).unwrap(), well_formed_metadata());
        assert_eq!(lock_metadata_admission_failure_count(), 0);
    }

    #[test]
    fn one_byte_over_cap_is_rejected_without_an_unbounded_read() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("over-cap.meta");
        let raw = padded_well_formed_metadata(MAX_LOCK_METADATA_BYTES + 1);
        fs::write(&path, raw).unwrap();

        let failure = read_lock_metadata(&path).unwrap_err();
        assert_eq!(failure.kind, "oversized");
        assert_eq!(
            failure.observed_bytes,
            Some((MAX_LOCK_METADATA_BYTES + 1) as u64)
        );
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn malformed_metadata_failure_does_not_retain_path_or_content_canaries() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path_canary = "private-lock-path-canary.meta";
        let content_canary = "AKIA-LOCK-CONTENT-CANARY";
        let path = tmp.path().join(path_canary);
        fs::write(&path, format!(r#"{{"pid":"{content_canary}""#)).unwrap();

        let failure = read_lock_metadata(&path).unwrap_err();
        let rendered = format!("{failure:?}");
        assert_eq!(failure.phase, "parse");
        assert_eq!(failure.kind, "invalid_schema");
        assert!(!rendered.contains(path_canary));
        assert!(!rendered.contains(content_canary));
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn metadata_directory_is_rejected_as_non_regular() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("directory.meta");
        fs::create_dir(&path).unwrap();

        let failure = read_lock_metadata(&path).unwrap_err();
        assert_eq!(failure.phase, "path_before_open");
        assert_eq!(failure.kind, "not_regular_file");
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn metadata_symlink_is_rejected_without_following_it() {
        use std::os::unix::fs::symlink;

        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target.meta");
        let link = tmp.path().join("link.meta");
        fs::write(&target, serde_json::to_vec(&well_formed_metadata()).unwrap()).unwrap();
        symlink(&target, &link).unwrap();

        let failure = read_lock_metadata(&link).unwrap_err();
        assert_eq!(failure.phase, "path_before_open");
        assert_eq!(failure.kind, "symlink");
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn metadata_namespace_replacement_after_open_is_rejected() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.meta");
        let moved = tmp.path().join("watcher.original.meta");
        let replacement = tmp.path().join("watcher.replacement.meta");
        let raw = serde_json::to_vec(&well_formed_metadata()).unwrap();
        fs::write(&path, &raw).unwrap();
        fs::write(&replacement, &raw).unwrap();

        let failure = read_lock_metadata_with_hook(&path, || {
            fs::rename(&path, &moved).unwrap();
            fs::rename(&replacement, &path).unwrap();
        })
        .unwrap_err();
        assert_eq!(failure.phase, "path_after_read");
        assert_eq!(failure.kind, "namespace_identity_changed");
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn same_length_metadata_mutation_after_open_is_rejected() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.meta");
        let raw = serde_json::to_vec(&well_formed_metadata()).unwrap();
        fs::write(&path, &raw).unwrap();

        let failure = read_lock_metadata_with_hook(&path, || {
            fs::write(&path, vec![b'x'; raw.len()]).unwrap();
        })
        .unwrap_err();
        assert_eq!(failure.phase, "read_identity");
        assert_eq!(failure.kind, "metadata_changed");
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn held_unknown_remains_distinct_from_free() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        assert_eq!(check_running(&lock_path), LockStatus::Free);

        let held = WatcherLock::acquire(&lock_path).unwrap();
        assert!(matches!(
            check_running(&lock_path),
            LockStatus::HeldKnown(_)
        ));
        held._metadata_file.set_len(0).unwrap();
        assert_eq!(check_running(&lock_path), LockStatus::HeldUnknown);

        drop(held);
        assert_eq!(check_running(&lock_path), LockStatus::Free);
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn main_lock_release_after_metadata_decode_cannot_return_held_known() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let held = WatcherLock::acquire(&lock_path).unwrap();

        let status = check_running_with_hooks(
            &lock_path,
            || fs2::FileExt::unlock(&held._lock_file).unwrap(),
            || {},
        );

        assert_eq!(status, LockStatus::Free);
        assert_eq!(lock_metadata_admission_failure_count(), 0);
    }

    #[test]
    fn sidecar_unlock_after_final_main_probe_cannot_return_held_known() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let held = WatcherLock::acquire(&lock_path).unwrap();

        let status = check_running_with_hooks(
            &lock_path,
            || {},
            || fs2::FileExt::unlock(&held._metadata_file).unwrap(),
        );

        assert_eq!(status, LockStatus::HeldUnknown);
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_replacement_after_final_main_probe_cannot_return_held_known() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let held = WatcherLock::acquire(&lock_path).unwrap();
        let meta_path = held.meta_path().to_path_buf();
        let moved_path = tmp.path().join("watcher.moved.meta.json");
        let replacement_path = tmp.path().join("watcher.replacement.meta.json");
        fs::write(
            &replacement_path,
            serde_json::to_vec(held.metadata()).unwrap(),
        )
        .unwrap();

        let status = check_running_with_hooks(
            &lock_path,
            || {},
            || {
                fs::rename(&meta_path, &moved_path).unwrap();
                fs::rename(&replacement_path, &meta_path).unwrap();
            },
        );

        assert_eq!(status, LockStatus::HeldUnknown);
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn held_lock_never_authorizes_unlocked_stale_metadata() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watcher.lock");
        let opened = open_lock_leaf_nofollow(&lock_path, true)
            .unwrap()
            .expect("lock leaf should be created");
        opened.file.try_lock_exclusive().unwrap();

        let stale = LockMetadata {
            pid: 42_424,
            started_at: 1_600_000_000,
            started_at_human: "unix:1600000000".to_string(),
            wa_version: "stale-holder".to_string(),
            instance_id: "99999999999999999999999999999999".to_string(),
        };
        fs::write(
            metadata_path(&lock_path),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();

        assert_eq!(check_running(&lock_path), LockStatus::HeldUnknown);
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn missing_file_bumps_counter_via_read_fail() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does_not_exist.meta");
        let meta = read_lock_metadata(&path);
        assert!(meta.is_err());
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn malformed_json_bumps_counter_via_parse_fail() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.lock.meta");
        fs::write(&path, "{ not valid json").unwrap();
        let meta = read_lock_metadata(&path);
        assert!(meta.is_err());
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn wrong_shape_bumps_counter_via_parse_fail() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.lock.meta");
        // valid JSON but missing every required LockMetadata field
        fs::write(&path, r#"{"unrelated": true}"#).unwrap();
        let meta = read_lock_metadata(&path);
        assert!(meta.is_err());
        assert_eq!(lock_metadata_admission_failure_count(), 1);
    }

    #[test]
    fn repeated_failures_bump_monotonically() {
        let _g = lock();
        reset_lock_metadata_admission_failure_count_for_test();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("watcher.lock.meta");
        fs::write(&path, "garbage").unwrap();
        for _ in 0..5 {
            let _ = read_lock_metadata(&path);
        }
        assert_eq!(lock_metadata_admission_failure_count(), 5);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(48))]

        // br-ft-zs9v0: any non-LockMetadata-shaped JSON or non-JSON
        // content must bump the counter exactly once and yield an error.
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
            reset_lock_metadata_admission_failure_count_for_test();
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("watcher.lock.meta");
            fs::write(&path, &shape).unwrap();
            let meta = read_lock_metadata(&path);
            proptest::prop_assert!(meta.is_err());
            proptest::prop_assert_eq!(lock_metadata_admission_failure_count(), 1);
        }
    }
}
