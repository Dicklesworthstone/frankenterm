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
            other => panic!("Expected AlreadyRunning, got: {other}"),
        }
    }

    #[test]
    fn read_existing_lock_error_with_corrupt_meta() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");
        let meta_path = metadata_path(&lock_path);

        fs::write(&meta_path, "not valid json").unwrap();

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
    }

    #[test]
    fn read_existing_lock_error_no_meta_file() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
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
        match lock_err {
            LockError::Io(ref e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
            _ => panic!("expected Io variant"),
        }
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
            other => panic!("expected AlreadyRunning, got: {other}"),
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
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("empty.lock");
        let meta_path = metadata_path(&lock_path);

        fs::write(&meta_path, "").unwrap();

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
    }

    #[test]
    fn read_existing_lock_error_partial_json() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("partial.lock");
        let meta_path = metadata_path(&lock_path);

        fs::write(&meta_path, r#"{"pid": 42"#).unwrap();

        assert!(matches!(
            read_existing_lock_error(&lock_path),
            LockError::AlreadyRunningNoMeta
        ));
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
