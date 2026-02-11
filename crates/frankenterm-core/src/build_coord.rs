//! Build coordination for concurrent agent workflows.
//!
//! When multiple agents target the same Rust project, competing `cargo build`
//! invocations can corrupt the target directory and waste resources compiling
//! the same crates in parallel. This module provides:
//!
//! - **Build locks**: Advisory file locks per project root so only one cargo
//!   invocation runs at a time.
//! - **Shared target directory**: Automatic `CARGO_TARGET_DIR` injection so all
//!   agents share a single build cache.
//! - **sccache detection**: Detects and configures sccache as `RUSTC_WRAPPER`
//!   when available.
//! - **Build coordination**: Wait-for-completion semantics — if another agent is
//!   building, wait for it to finish, then reuse artifacts.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors from build coordination operations.
#[derive(Error, Debug)]
pub enum BuildCoordError {
    /// Another build is already running for this project.
    #[error("build already running (pid: {pid}, project: {project}, started: {started_at})")]
    BuildInProgress {
        pid: u32,
        project: String,
        started_at: String,
    },

    /// Timed out waiting for another build to complete.
    #[error("timed out after {elapsed:?} waiting for build lock on {project}")]
    WaitTimeout { project: String, elapsed: Duration },

    /// I/O error during lock operations.
    #[error("build coord I/O error: {0}")]
    Io(#[from] io::Error),

    /// Failed to serialize/deserialize metadata.
    #[error("build coord metadata error: {0}")]
    Metadata(#[from] serde_json::Error),

    /// Project root not found or not a cargo project.
    #[error("not a cargo project: {0}")]
    NotCargoProject(PathBuf),
}

// ── Configuration ───────────────────────────────────────────────────────────

/// Configuration for build coordination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildCoordConfig {
    /// Enable build coordination (default: true).
    pub enabled: bool,

    /// Maximum time to wait for another build to finish before giving up.
    pub wait_timeout: Duration,

    /// Poll interval when waiting for a build lock.
    pub poll_interval: Duration,

    /// Use a shared target directory for all agents on the same project.
    pub shared_target_dir: bool,

    /// Path override for the shared target directory.
    /// If None, uses `<project_root>/target` (the default).
    pub target_dir_override: Option<PathBuf>,

    /// Automatically detect and configure sccache.
    pub auto_sccache: bool,

    /// Directory for build lock files.
    /// If None, uses `<project_root>/.ft/build/`.
    pub lock_dir_override: Option<PathBuf>,
}

impl Default for BuildCoordConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            wait_timeout: Duration::from_secs(600), // 10 minutes
            poll_interval: Duration::from_millis(500),
            shared_target_dir: true,
            target_dir_override: None,
            auto_sccache: true,
            lock_dir_override: None,
        }
    }
}

// ── Build lock metadata ─────────────────────────────────────────────────────

/// Metadata about an active build lock holder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildLockMetadata {
    /// PID of the process holding the lock.
    pub pid: u32,
    /// Cargo subcommand being run (build, check, test, bench, clippy).
    pub cargo_command: String,
    /// Project root path.
    pub project_root: String,
    /// Unix timestamp when the build started.
    pub started_at: u64,
    /// Human-readable start time.
    pub started_at_human: String,
    /// FrankenTerm version.
    pub ft_version: String,
    /// Agent name (if known).
    pub agent_name: Option<String>,
    /// Pane ID where the build is running (if known).
    pub pane_id: Option<u64>,
}

impl BuildLockMetadata {
    fn new(cargo_command: &str, project_root: &Path) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        Self {
            pid: std::process::id(),
            cargo_command: cargo_command.to_string(),
            project_root: project_root.display().to_string(),
            started_at: now,
            started_at_human: format!("unix:{now}"),
            ft_version: crate::VERSION.to_string(),
            agent_name: None,
            pane_id: None,
        }
    }
}

// ── Build lock guard ────────────────────────────────────────────────────────

/// An acquired build lock. Released automatically on drop.
pub struct BuildLock {
    _lock_file: File,
    lock_path: PathBuf,
    meta_path: PathBuf,
    project_root: PathBuf,
}

impl BuildLock {
    /// Attempt to acquire the build lock for a project (non-blocking).
    ///
    /// Returns `Ok(BuildLock)` if acquired, `Err(BuildInProgress)` if held.
    pub fn try_acquire(
        project_root: &Path,
        cargo_command: &str,
        config: &BuildCoordConfig,
    ) -> Result<Self, BuildCoordError> {
        let lock_dir = lock_dir(project_root, config);
        fs::create_dir_all(&lock_dir)?;

        let lock_path = lock_dir.join("cargo.lock");
        let meta_path = lock_dir.join("cargo.lock.meta.json");

        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        match lock_file.try_lock_exclusive() {
            Ok(()) => {
                let lock = Self {
                    _lock_file: lock_file,
                    lock_path,
                    meta_path,
                    project_root: project_root.to_path_buf(),
                };
                lock.write_metadata(cargo_command)?;
                debug!(
                    project = %project_root.display(),
                    command = cargo_command,
                    "Acquired build lock"
                );
                Ok(lock)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let meta = read_build_metadata(&meta_path);
                match meta {
                    Some(m) => Err(BuildCoordError::BuildInProgress {
                        pid: m.pid,
                        project: m.project_root,
                        started_at: m.started_at_human,
                    }),
                    None => Err(BuildCoordError::BuildInProgress {
                        pid: 0,
                        project: project_root.display().to_string(),
                        started_at: "unknown".to_string(),
                    }),
                }
            }
            Err(e) => Err(BuildCoordError::Io(e)),
        }
    }

    /// Acquire the build lock, waiting up to `config.wait_timeout` if held.
    ///
    /// Polls at `config.poll_interval` until the lock becomes available or
    /// the timeout expires.
    pub fn acquire_with_wait(
        project_root: &Path,
        cargo_command: &str,
        config: &BuildCoordConfig,
    ) -> Result<Self, BuildCoordError> {
        let start = Instant::now();

        loop {
            match Self::try_acquire(project_root, cargo_command, config) {
                Ok(lock) => return Ok(lock),
                Err(BuildCoordError::BuildInProgress { pid, project, .. }) => {
                    let elapsed = start.elapsed();
                    if elapsed >= config.wait_timeout {
                        return Err(BuildCoordError::WaitTimeout { project, elapsed });
                    }

                    info!(
                        pid,
                        project = %project,
                        elapsed_secs = elapsed.as_secs(),
                        timeout_secs = config.wait_timeout.as_secs(),
                        "Waiting for build lock..."
                    );
                    std::thread::sleep(config.poll_interval);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn write_metadata(&self, cargo_command: &str) -> Result<(), BuildCoordError> {
        let metadata = BuildLockMetadata::new(cargo_command, &self.project_root);
        let json = serde_json::to_string_pretty(&metadata)?;
        let mut file = File::create(&self.meta_path)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// Get the project root this lock covers.
    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.meta_path) {
            if e.kind() != io::ErrorKind::NotFound {
                warn!(
                    meta_path = %self.meta_path.display(),
                    error = %e,
                    "Failed to remove build lock metadata"
                );
            }
        }
        debug!(
            project = %self.project_root.display(),
            "Released build lock"
        );
    }
}

// ── Build environment ───────────────────────────────────────────────────────

/// Environment variables to inject for coordinated cargo builds.
#[derive(Debug, Clone, Default)]
pub struct BuildEnv {
    /// Environment variables to set before running cargo.
    pub vars: HashMap<String, String>,
}

impl BuildEnv {
    /// Detect optimal build environment for a project.
    ///
    /// Sets `CARGO_TARGET_DIR`, `RUSTC_WRAPPER` (if sccache found), and other
    /// performance-relevant variables.
    pub fn detect(project_root: &Path, config: &BuildCoordConfig) -> Self {
        let mut vars = HashMap::new();

        // Shared target directory
        if config.shared_target_dir {
            let target_dir = config
                .target_dir_override
                .clone()
                .unwrap_or_else(|| project_root.join("target"));

            vars.insert(
                "CARGO_TARGET_DIR".to_string(),
                target_dir.display().to_string(),
            );
        }

        // sccache detection
        if config.auto_sccache {
            if let Some(sccache_path) = detect_sccache() {
                // Only set if not already overridden
                if std::env::var("RUSTC_WRAPPER").is_err() {
                    vars.insert("RUSTC_WRAPPER".to_string(), sccache_path);
                }
            }
        }

        // Incremental compilation (enabled by default for debug, but explicit)
        if std::env::var("CARGO_INCREMENTAL").is_err() {
            vars.insert("CARGO_INCREMENTAL".to_string(), "1".to_string());
        }

        BuildEnv { vars }
    }

    /// Apply the build environment to a [`std::process::Command`].
    ///
    /// This is the safe way to inject the build environment — instead of
    /// modifying the process environment (which requires unsafe in Rust 2024),
    /// we set env vars on the command that will be spawned.
    pub fn apply_to_command(&self, cmd: &mut std::process::Command) {
        for (key, value) in &self.vars {
            cmd.env(key, value);
        }
    }
}

// ── Build status checking ───────────────────────────────────────────────────

/// Check if a build is currently running for a project.
///
/// Returns `Some(metadata)` if a build lock is held, `None` otherwise.
#[must_use]
pub fn check_build_running(
    project_root: &Path,
    config: &BuildCoordConfig,
) -> Option<BuildLockMetadata> {
    let lock_dir = lock_dir(project_root, config);
    let lock_path = lock_dir.join("cargo.lock");
    let meta_path = lock_dir.join("cargo.lock.meta.json");

    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(&lock_path)
        .ok()?;

    match lock_file.try_lock_exclusive() {
        Ok(()) => {
            // Got the lock — nothing was holding it
            drop(lock_file);
            None
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            // Lock is held
            read_build_metadata(&meta_path)
        }
        Err(_) => None,
    }
}

/// Detect the project root for a directory by walking up to find Cargo.toml.
///
/// Returns the directory containing the root `Cargo.toml` (workspace root if
/// it's a workspace, otherwise the package root).
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };

    let mut candidate = None;

    loop {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists() {
            // Check if this is a workspace root
            if let Ok(contents) = fs::read_to_string(&cargo_toml) {
                if contents.contains("[workspace]") {
                    return Some(dir);
                }
            }
            // Remember this as a candidate (package root)
            if candidate.is_none() {
                candidate = Some(dir.clone());
            }
        }

        if !dir.pop() {
            break;
        }
    }

    candidate
}

/// Check if a command string looks like a cargo build command.
///
/// Returns the cargo subcommand if detected (e.g., "build", "check", "test").
#[must_use]
pub fn detect_cargo_command(command: &str) -> Option<&str> {
    let trimmed = command.trim();

    // Direct cargo invocations
    if let Some(rest) = trimmed.strip_prefix("cargo ") {
        let subcommand = rest.split_whitespace().next()?;
        return match subcommand {
            "build" | "b" => Some("build"),
            "check" | "c" => Some("check"),
            "test" | "t" => Some("test"),
            "bench" => Some("bench"),
            "clippy" => Some("clippy"),
            "run" | "r" => Some("run"),
            "doc" => Some("doc"),
            _ => None,
        };
    }

    // cargo-nextest
    if trimmed.starts_with("cargo-nextest") || trimmed.contains("cargo nextest") {
        return Some("test");
    }

    None
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Detect sccache binary path.
fn detect_sccache() -> Option<String> {
    // Check common paths
    for candidate in &[
        "sccache",
        "/usr/local/bin/sccache",
        "/opt/homebrew/bin/sccache",
    ] {
        if which_exists(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Check if a command exists on PATH.
fn which_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Compute the lock directory for a project.
fn lock_dir(project_root: &Path, config: &BuildCoordConfig) -> PathBuf {
    config
        .lock_dir_override
        .clone()
        .unwrap_or_else(|| project_root.join(".ft").join("build"))
}

/// Read build lock metadata from the sidecar file.
fn read_build_metadata(meta_path: &Path) -> Option<BuildLockMetadata> {
    fs::read_to_string(meta_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"test-proj\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        tmp
    }

    fn setup_workspace() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        let crate_dir = tmp.path().join("crates").join("foo");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn acquire_and_release_build_lock() {
        let tmp = setup_project();
        let config = BuildCoordConfig::default();

        let lock = BuildLock::try_acquire(tmp.path(), "build", &config).unwrap();
        let meta_path = lock.meta_path.clone();
        assert!(meta_path.exists());

        drop(lock);
        assert!(!meta_path.exists());
    }

    #[test]
    fn double_acquire_fails() {
        let tmp = setup_project();
        let config = BuildCoordConfig::default();

        let _lock1 = BuildLock::try_acquire(tmp.path(), "build", &config).unwrap();
        let result = BuildLock::try_acquire(tmp.path(), "check", &config);
        assert!(matches!(
            result,
            Err(BuildCoordError::BuildInProgress { .. })
        ));
    }

    #[test]
    fn release_allows_reacquire() {
        let tmp = setup_project();
        let config = BuildCoordConfig::default();

        let lock = BuildLock::try_acquire(tmp.path(), "build", &config).unwrap();
        drop(lock);

        // Should succeed now
        let _lock2 = BuildLock::try_acquire(tmp.path(), "test", &config).unwrap();
    }

    #[test]
    fn check_build_running_detects_active() {
        let tmp = setup_project();
        let config = BuildCoordConfig::default();

        assert!(check_build_running(tmp.path(), &config).is_none());

        let _lock = BuildLock::try_acquire(tmp.path(), "build", &config).unwrap();
        let meta = check_build_running(tmp.path(), &config);
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.pid, std::process::id());
        assert_eq!(meta.cargo_command, "build");
    }

    #[test]
    fn metadata_roundtrip() {
        let tmp = setup_project();
        let config = BuildCoordConfig::default();

        let _lock = BuildLock::try_acquire(tmp.path(), "test", &config).unwrap();

        let meta_path = lock_dir(tmp.path(), &config).join("cargo.lock.meta.json");
        let contents = fs::read_to_string(&meta_path).unwrap();
        let meta: BuildLockMetadata = serde_json::from_str(&contents).unwrap();

        assert_eq!(meta.pid, std::process::id());
        assert_eq!(meta.cargo_command, "test");
        assert!(!meta.ft_version.is_empty());
        assert!(meta.started_at > 0);
    }

    #[test]
    fn find_project_root_simple() {
        let tmp = setup_project();
        let root = find_project_root(tmp.path());
        assert_eq!(root, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn find_project_root_workspace() {
        let tmp = setup_workspace();
        let crate_dir = tmp.path().join("crates").join("foo");

        // From a subcrate, should find workspace root
        let root = find_project_root(&crate_dir);
        assert_eq!(root, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn find_project_root_not_cargo() {
        let tmp = TempDir::new().unwrap();
        // No Cargo.toml anywhere
        let root = find_project_root(tmp.path());
        assert!(root.is_none());
    }

    #[test]
    fn detect_cargo_commands() {
        assert_eq!(detect_cargo_command("cargo build"), Some("build"));
        assert_eq!(detect_cargo_command("cargo b"), Some("build"));
        assert_eq!(detect_cargo_command("cargo check"), Some("check"));
        assert_eq!(detect_cargo_command("cargo c"), Some("check"));
        assert_eq!(detect_cargo_command("cargo test"), Some("test"));
        assert_eq!(detect_cargo_command("cargo t"), Some("test"));
        assert_eq!(detect_cargo_command("cargo bench"), Some("bench"));
        assert_eq!(detect_cargo_command("cargo clippy"), Some("clippy"));
        assert_eq!(detect_cargo_command("cargo run"), Some("run"));
        assert_eq!(detect_cargo_command("cargo doc"), Some("doc"));
        assert_eq!(detect_cargo_command("cargo build --release"), Some("build"));
        assert_eq!(
            detect_cargo_command("cargo test -- --nocapture"),
            Some("test")
        );

        // Non-build commands
        assert_eq!(detect_cargo_command("cargo fmt"), None);
        assert_eq!(detect_cargo_command("cargo update"), None);
        assert_eq!(detect_cargo_command("ls -la"), None);
        assert_eq!(detect_cargo_command("rustc --version"), None);

        // Nextest
        assert_eq!(detect_cargo_command("cargo nextest run"), Some("test"));
    }

    #[test]
    fn build_env_sets_target_dir() {
        let tmp = setup_project();
        let config = BuildCoordConfig {
            shared_target_dir: true,
            auto_sccache: false,
            ..Default::default()
        };

        let env = BuildEnv::detect(tmp.path(), &config);
        let target_dir = env.vars.get("CARGO_TARGET_DIR").unwrap();
        assert!(target_dir.ends_with("target"));
    }

    #[test]
    fn build_env_respects_override() {
        let tmp = setup_project();
        let config = BuildCoordConfig {
            shared_target_dir: true,
            target_dir_override: Some(PathBuf::from("/tmp/shared-target")),
            auto_sccache: false,
            ..Default::default()
        };

        let env = BuildEnv::detect(tmp.path(), &config);
        assert_eq!(
            env.vars.get("CARGO_TARGET_DIR").unwrap(),
            "/tmp/shared-target"
        );
    }

    #[test]
    fn build_env_disabled_target_dir() {
        let tmp = setup_project();
        let config = BuildCoordConfig {
            shared_target_dir: false,
            auto_sccache: false,
            ..Default::default()
        };

        let env = BuildEnv::detect(tmp.path(), &config);
        assert!(!env.vars.contains_key("CARGO_TARGET_DIR"));
    }

    #[test]
    fn wait_timeout_with_short_timeout() {
        let tmp = setup_project();
        let config = BuildCoordConfig {
            wait_timeout: Duration::from_millis(100),
            poll_interval: Duration::from_millis(20),
            ..Default::default()
        };

        let _lock = BuildLock::try_acquire(tmp.path(), "build", &config).unwrap();

        // Second acquire with wait should timeout
        let result = BuildLock::acquire_with_wait(tmp.path(), "check", &config);
        assert!(matches!(result, Err(BuildCoordError::WaitTimeout { .. })));
    }

    #[test]
    fn config_default_values() {
        let config = BuildCoordConfig::default();
        assert!(config.enabled);
        assert_eq!(config.wait_timeout, Duration::from_secs(600));
        assert_eq!(config.poll_interval, Duration::from_millis(500));
        assert!(config.shared_target_dir);
        assert!(config.auto_sccache);
        assert!(config.target_dir_override.is_none());
        assert!(config.lock_dir_override.is_none());
    }

    #[test]
    fn lock_dir_default() {
        let project = PathBuf::from("/home/user/project");
        let config = BuildCoordConfig::default();
        let dir = lock_dir(&project, &config);
        assert_eq!(dir, PathBuf::from("/home/user/project/.ft/build"));
    }

    #[test]
    fn lock_dir_override() {
        let project = PathBuf::from("/home/user/project");
        let config = BuildCoordConfig {
            lock_dir_override: Some(PathBuf::from("/tmp/ft-locks")),
            ..Default::default()
        };
        let dir = lock_dir(&project, &config);
        assert_eq!(dir, PathBuf::from("/tmp/ft-locks"));
    }
}
