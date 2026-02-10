//! Single-instance lock for the watcher daemon.
//!
//! Uses OS-level file locking (via fs2) to ensure only one watcher instance
//! runs at a time. A sidecar metadata file records diagnostic information
//! for debugging.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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

/// Read metadata from an existing lock to provide a helpful error message.
#[allow(clippy::option_if_let_else)]
fn read_existing_lock_error(lock_path: &Path) -> LockError {
    let meta_path = metadata_path(lock_path);
    match fs::read_to_string(&meta_path) {
        Ok(contents) => match serde_json::from_str::<LockMetadata>(&contents) {
            Ok(meta) => LockError::AlreadyRunning {
                pid: meta.pid,
                started_at: meta.started_at_human,
            },
            Err(_) => LockError::AlreadyRunningNoMeta,
        },
        Err(_) => LockError::AlreadyRunningNoMeta,
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
            // Lock is held, try to read metadata
            let meta_path = metadata_path(lock_path);
            fs::read_to_string(&meta_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
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
}
