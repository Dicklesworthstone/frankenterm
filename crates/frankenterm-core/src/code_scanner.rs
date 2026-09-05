//! Policy-gated, bounded static Rust scanning through the real `ubs` CLI.
//!
//! Selected source bytes are read through pinned directory capabilities and
//! copied to a private retained snapshot. UBS receives that snapshot, never a
//! mutable project path. Findings are data; this adapter executes no fixes.
//!
//! Feature-gated behind `subprocess-bridge`.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::info;

use crate::cx::Cx;
use crate::policy::{ActionKind, ActorKind, PolicyEngine, PolicyInput, Redactor};
use crate::runtime_async::process::{
    Command, CommandCancellation, CommandCancelled, CommandOutputCaptureIncomplete,
    CommandOutputLimitExceeded, CommandProcessCleanupIncomplete, CommandTimedOut,
};
use crate::subprocess_bridge::SubprocessBridge;

const SUPPORTED_UBS_VERSION: &str = "5.2.42";
const MAX_SCAN_FILES: usize = 256;
const MAX_SCAN_ENTRIES: usize = 4096;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const MAX_STDOUT_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTIC_CHARS: usize = 2048;

/// The supported profile deliberately excludes Cargo-driven categories.
pub const STATIC_RUST_PROFILE: &str = "ubs-static-rust-v1";

/// Scope is always relative to an explicitly supplied project root. Staged
/// and diff select Git path names, then scan their current working-tree bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScanScope {
    Path { path: PathBuf },
    Staged,
    Diff,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanRequest {
    pub project_root: PathBuf,
    pub scope: ScanScope,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

const fn default_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanStage {
    Admission,
    Version,
    Selection,
    Snapshot,
    Scanner,
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScanErrorKind {
    #[error("invalid scan request: {reason}")]
    InvalidRequest { reason: &'static str },
    #[error("scanner execution denied by policy")]
    PolicyDenied,
    #[error("scanner execution requires policy approval")]
    ApprovalRequired,
    #[error("workspace policy fence is held by another effect or transition")]
    PolicyBusy,
    #[error("required executable is unavailable")]
    Unavailable,
    #[error("scanner version is unsupported")]
    VersionMismatch,
    #[error("selected path escapes its project capability")]
    PathEscape,
    #[error("selected entry is a symlink or unsupported file type")]
    UnsupportedEntry,
    #[error("scope contains no qualified Rust source files")]
    NoQualifiedInputs,
    #[error("scope exceeds the file, entry, depth or byte limit")]
    ScopeTooLarge,
    #[error("process exited unsuccessfully")]
    NonzeroExit { stage: ScanStage, code: Option<i32> },
    #[error("scan deadline exceeded")]
    Timeout,
    #[error("scan cancelled")]
    Cancelled,
    #[error("process output exceeded its byte limit")]
    OutputTooLarge {
        stream: String,
        observed: usize,
        limit: usize,
    },
    #[error("process output capture is incomplete")]
    CaptureIncomplete,
    #[error("process cleanup is incomplete")]
    CleanupIncomplete,
    #[error("process supervisor settlement is unconfirmed")]
    SupervisorUnsettled,
    #[error("process supervisor capacity is exhausted")]
    Busy,
    #[error("malformed scanner output: {reason}")]
    MalformedOutput { reason: &'static str },
    #[error("scanner reported incomplete or inconsistent coverage")]
    PartialResult,
    #[error("scan I/O failed ({kind})")]
    Io { kind: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanProcessDiagnostic {
    pub stage: ScanStage,
    pub exit_code: Option<i32>,
    pub spawned: bool,
    pub supervisor_settled: bool,
    pub stdout_bytes: Option<usize>,
    pub stderr_bytes: Option<usize>,
    /// Bounded and redacted; never logged automatically.
    pub stderr_excerpt: Option<String>,
}

#[derive(Debug, Error, Serialize)]
#[error("{kind}")]
pub struct ScanError {
    pub kind: ScanErrorKind,
    pub stage: ScanStage,
    pub diagnostics: Vec<ScanProcessDiagnostic>,
    pub retained_snapshot: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanInput {
    pub origin_path: PathBuf,
    pub bytes: usize,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanOutcome {
    pub profile: &'static str,
    pub supported_language: &'static str,
    pub excluded_checks: [&'static str; 3],
    pub excluded_non_rust_files: usize,
    pub root_hash: String,
    pub scope_hash: String,
    pub scanner_version: String,
    pub scanner_exit_code: i32,
    pub classification: ScanClassification,
    pub report: ScanReport,
    pub inputs: Vec<ScanInput>,
    pub retained_snapshot: PathBuf,
    pub diagnostics: Vec<ScanProcessDiagnostic>,
}

struct ScanProgress {
    stage: ScanStage,
    diagnostics: Vec<ScanProcessDiagnostic>,
    snapshot: Option<PathBuf>,
}

/// Restore configured scanner policy without initializing workspace state.
/// The returned fence must remain alive until every admitted scan process has
/// settled. Existing databases share the operator transition fence. Absence is
/// admitted only after a pinned-parent check; it represents admission before
/// the first workspace state exists, not fencing of later state creation.
///
/// SQLite accepts a pathname, so this uses the existing trusted-workspace
/// model: cooperative writers take the fence, while malicious same-UID
/// namespace swaps that replace and restore an inode during SQLite open are
/// outside that model. Symlinks, hardlinks and observed replacements fail
/// closed. No database bytes are copied or logged.
pub async fn load_scan_policy(
    cx: &Cx,
    config: &crate::config::Config,
    workspace_root: &Path,
    db_path: &Path,
    deadline: Instant,
) -> Result<
    (
        PolicyEngine,
        Option<crate::policy_kill_switch_state::KillSwitchFence>,
    ),
    ScanErrorKind,
> {
    let safety = config.safety.clone();
    let tuning = config.tuning.clone();
    let workspace_root = workspace_root.to_path_buf();
    let db_path = db_path.to_path_buf();
    blocking_scan_io(cx, deadline, move |source_cx| {
        let source = ScanPolicySource::prepare(&workspace_root, &db_path)?;
        let mut policy = PolicyEngine::from_safety_config(&safety).with_tuning(&tuning);
        let fence = source.restore(source_cx, deadline, &mut policy)?;
        Ok((policy, fence))
    })
    .await
}

struct ScanPolicySource {
    parent: Dir,
    parent_path: PathBuf,
    leaf: PathBuf,
    metadata: Option<cap_std::fs::Metadata>,
}

impl ScanPolicySource {
    fn prepare(workspace_root: &Path, db_path: &Path) -> Result<Self, ScanErrorKind> {
        if !workspace_root.is_absolute()
            || !db_path.is_absolute()
            || db_path.as_os_str().len() > 4096
            || db_path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(policy_source_failure());
        }
        // Resolve only the operator-authorized workspace anchor (including
        // platform aliases). Explicit external DB paths start at filesystem
        // root and receive the same nofollow component walk.
        let (anchor, relative) = match db_path.strip_prefix(workspace_root) {
            Ok(relative) => (
                std::fs::canonicalize(workspace_root).map_err(|_| policy_source_failure())?,
                relative.to_path_buf(),
            ),
            Err(_) => {
                let anchor = db_path
                    .ancestors()
                    .last()
                    .ok_or_else(policy_source_failure)?;
                (
                    anchor.to_path_buf(),
                    db_path
                        .strip_prefix(anchor)
                        .map_err(|_| policy_source_failure())?
                        .to_path_buf(),
                )
            }
        };
        let parts = relative.components().collect::<Vec<_>>();
        if parts.is_empty() || parts.len() > 64 {
            return Err(policy_source_failure());
        }
        let mut parent = open_root_nofollow(&anchor).map_err(|_| policy_source_failure())?;
        let mut parent_path = anchor;
        for (index, part) in parts.iter().enumerate() {
            let Component::Normal(name) = part else {
                return Err(policy_source_failure());
            };
            let leaf = PathBuf::from(name);
            match parent.symlink_metadata(&leaf) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Self {
                        parent,
                        parent_path,
                        leaf,
                        metadata: None,
                    });
                }
                Ok(metadata) if index + 1 == parts.len() => {
                    use cap_fs_ext::MetadataExt as _;
                    if !metadata.is_file()
                        || metadata.file_type().is_symlink()
                        || metadata.nlink() != 1
                    {
                        return Err(policy_source_failure());
                    }
                    return Ok(Self {
                        parent,
                        parent_path,
                        leaf,
                        metadata: Some(metadata),
                    });
                }
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    parent = parent
                        .open_dir_nofollow(&leaf)
                        .map_err(|_| policy_source_failure())?;
                    parent_path.push(leaf);
                }
                _ => return Err(policy_source_failure()),
            }
        }
        Err(policy_source_failure())
    }

    fn verify(&self) -> Result<(), ScanErrorKind> {
        use cap_fs_ext::MetadataExt as _;
        let reopened =
            open_root_nofollow(&self.parent_path).map_err(|_| policy_source_failure())?;
        let before = self
            .parent
            .dir_metadata()
            .map_err(|_| policy_source_failure())?;
        let after = reopened
            .dir_metadata()
            .map_err(|_| policy_source_failure())?;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(policy_source_failure());
        }
        match (&self.metadata, self.parent.symlink_metadata(&self.leaf)) {
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            (Some(before), Ok(after))
                if after.is_file()
                    && !after.file_type().is_symlink()
                    && after.nlink() == 1
                    && before.dev() == after.dev()
                    && before.ino() == after.ino() =>
            {
                Ok(())
            }
            _ => Err(policy_source_failure()),
        }
    }

    fn verify_auxiliaries(&self) -> Result<(), ScanErrorKind> {
        use cap_fs_ext::MetadataExt as _;
        for suffix in ["-wal", "-shm", "-journal", ".policy-kill-switch.lock"] {
            let mut name = self.leaf.clone().into_os_string();
            name.push(suffix);
            match self.parent.symlink_metadata(Path::new(&name)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(metadata)
                    if metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.nlink() == 1 => {}
                _ => return Err(policy_source_failure()),
            }
        }
        Ok(())
    }

    fn restore(
        &self,
        cx: &Cx,
        deadline: Instant,
        policy: &mut PolicyEngine,
    ) -> Result<Option<crate::policy_kill_switch_state::KillSwitchFence>, ScanErrorKind> {
        use crate::policy_kill_switch_state::{
            KillSwitchStateError, acquire_kill_switch_fence, restore_kill_switch_from_backend,
        };
        checkpoint(cx, deadline)?;
        self.verify()?;
        if self.metadata.is_none() {
            return Ok(None);
        }
        self.verify_auxiliaries()?;
        let path = self.parent_path.join(&self.leaf);
        let fence = acquire_kill_switch_fence(&path).map_err(|error| match error {
            KillSwitchStateError::FencePending => ScanErrorKind::PolicyBusy,
            _ => policy_source_failure(),
        })?;
        self.verify()?;
        self.verify_auxiliaries()?;
        checkpoint(cx, deadline)?;
        let connection = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| policy_source_failure())?;
        connection
            .busy_timeout(Duration::ZERO)
            .map_err(|_| policy_source_failure())?;
        connection
            .pragma_update(None, "trusted_schema", false)
            .map_err(|_| policy_source_failure())?;
        let progress_cx = cx.clone();
        connection.progress_handler(
            1000,
            Some(move || checkpoint(&progress_cx, deadline).is_err()),
        );
        let _: i64 = connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .map_err(|_| {
                checkpoint(cx, deadline)
                    .err()
                    .unwrap_or_else(policy_source_failure)
            })?;
        self.verify()?;
        self.verify_auxiliaries()?;
        let backend = crate::storage_backend_trait::RusqliteBackend::new(connection);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| policy_source_failure())?;
        let now_ms = u64::try_from(now.as_millis()).map_err(|_| policy_source_failure())?;
        let restore = restore_kill_switch_from_backend(policy, &backend, now_ms);
        self.verify()?;
        self.verify_auxiliaries()?;
        checkpoint(cx, deadline)?;
        tracing::info!(
            surface = "code-scan",
            restore = restore.label(),
            "scanner policy restored"
        );
        Ok(Some(fence))
    }
}

fn policy_source_failure() -> ScanErrorKind {
    ScanErrorKind::Io {
        kind: "policy_state_unavailable".to_string(),
    }
}

// =============================================================================
// Types
// =============================================================================

/// Severity of a scan finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingSeverity {
    Info,
    Warning,
    Critical,
}

impl std::fmt::Display for FindingSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Info => f.write_str("info"),
            Self::Warning => f.write_str("warning"),
            Self::Critical => f.write_str("critical"),
        }
    }
}

/// A single finding from the scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanFinding {
    pub severity: FindingSeverity,
    pub category: String,
    pub message: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub suggestion: Option<String>,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Per-scanner summary from ubs JSON output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannerSummary {
    pub language: Option<String>,
    pub files: usize,
    pub critical: usize,
    pub warning: usize,
    pub info: usize,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Aggregated scan totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanTotals {
    pub critical: usize,
    pub warning: usize,
    pub info: usize,
    pub files: usize,
}

impl ScanTotals {
    /// Total finding count across all severities.
    pub fn total(&self) -> Option<usize> {
        self.critical
            .checked_add(self.warning)?
            .checked_add(self.info)
    }

    /// Whether any critical findings exist.
    #[must_use]
    pub fn has_critical(&self) -> bool {
        self.critical > 0
    }
}

/// Full scan report from ubs `--format=json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    pub project: Option<String>,
    pub scanners: Vec<ScannerSummary>,
    pub totals: ScanTotals,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// =============================================================================
// Bridge
// =============================================================================

/// High-level code scanner bridge wrapping the `ubs` CLI.
#[derive(Debug, Clone)]
pub struct CodeScanner {
    bridge: SubprocessBridge<ScanReport>,
    snapshot_parent: Option<PathBuf>,
}

impl CodeScanner {
    /// Create a new scanner looking for `ubs` in PATH.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bridge: SubprocessBridge::new("ubs").with_search_paths(Vec::<PathBuf>::new()),
            snapshot_parent: None,
        }
    }

    /// Check whether the `ubs` binary can be found.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.bridge.is_available()
    }

    /// Select an operator-configured scanner executable, never a command from
    /// an input finding. Relative executable paths are resolved before cwd changes.
    pub fn with_binary(mut self, binary: &Path) -> Result<Self, ScanErrorKind> {
        let binary = binary.to_str().ok_or(ScanErrorKind::InvalidRequest {
            reason: "scanner executable path must be UTF-8",
        })?;
        self.bridge = SubprocessBridge::new(binary).with_search_paths(Vec::<PathBuf>::new());
        Ok(self)
    }

    /// Set the parent for private retained snapshots. No snapshot is deleted,
    /// including on errors or cancellation. The caller owns retention policy.
    #[must_use]
    pub fn with_snapshot_parent(mut self, parent: PathBuf) -> Self {
        self.snapshot_parent = Some(parent);
        self
    }

    /// Run one admitted scan under the original caller Cx and one absolute
    /// deadline, including version discovery, Git selection and snapshot reads.
    /// The caller's absolute deadline is never extended; request.timeout_ms may
    /// only impose a shorter limit from library admission.
    pub async fn scan(
        &self,
        cx: &Cx,
        policy: &mut PolicyEngine,
        actor: ActorKind,
        request: &ScanRequest,
        deadline: Instant,
    ) -> Result<ScanOutcome, ScanError> {
        let mut progress = ScanProgress {
            stage: ScanStage::Admission,
            diagnostics: Vec::new(),
            snapshot: None,
        };
        match self
            .scan_inner(cx, policy, actor, request, deadline, &mut progress)
            .await
        {
            Ok(mut outcome) => {
                outcome.diagnostics = progress.diagnostics;
                Ok(outcome)
            }
            Err(kind) => {
                tracing::warn!(profile = STATIC_RUST_PROFILE, stage = ?progress.stage,
                    reason = %kind, "code scan refused or failed");
                Err(ScanError {
                    kind,
                    stage: progress.stage,
                    diagnostics: progress.diagnostics,
                    retained_snapshot: progress.snapshot,
                })
            }
        }
    }

    async fn scan_inner(
        &self,
        cx: &Cx,
        policy: &mut PolicyEngine,
        actor: ActorKind,
        request: &ScanRequest,
        deadline: Instant,
        progress: &mut ScanProgress,
    ) -> Result<ScanOutcome, ScanErrorKind> {
        if !(1..=120_000).contains(&request.timeout_ms)
            || !request.project_root.is_absolute()
            || request.project_root.as_os_str().len() > 4096
        {
            return Err(ScanErrorKind::InvalidRequest {
                reason: "absolute root and timeout_ms in 1..=120000 required",
            });
        }
        let deadline = deadline.min(Instant::now() + Duration::from_millis(request.timeout_ms));
        checkpoint(cx, deadline)?;
        let requested_root = request.project_root.clone();
        let (root, root_cap) = blocking_scan_io(cx, deadline, move |_| {
            let root = std::fs::canonicalize(requested_root).map_err(io_failure)?;
            let directory = open_root_nofollow(&root)?;
            Ok((root, directory))
        })
        .await?;
        let root_text = root.to_str().ok_or(ScanErrorKind::InvalidRequest {
            reason: "project root must be UTF-8",
        })?;
        let root_hash = digest(root.as_os_str().as_encoded_bytes());
        let scope_hash = digest(&serde_json::to_vec(&request.scope).map_err(|_| {
            ScanErrorKind::InvalidRequest {
                reason: "scope must be UTF-8",
            }
        })?);
        let input = PolicyInput::new(ActionKind::ExecCommand, actor)
            .with_domain("code-scanner")
            .with_pane_cwd(root_text)
            .with_command_text("ubs --only=rust --skip=12,13,14 --ci --format=json")
            .with_text_summary(format!(
                "{STATIC_RUST_PROFILE} root={root_hash} scope={scope_hash}"
            ));
        let decision = policy.authorize(&input);
        if !decision.is_allowed() {
            return Err(if decision.requires_approval() {
                ScanErrorKind::ApprovalRequired
            } else {
                ScanErrorKind::PolicyDenied
            });
        }
        checkpoint(cx, deadline)?;
        info!(profile = STATIC_RUST_PROFILE, %root_hash, %scope_hash, "code scan admitted");
        progress.stage = ScanStage::Version;
        let bridge = self.bridge.clone();
        let binary = blocking_scan_io(cx, deadline, move |_| {
            let binary = bridge
                .resolve_binary()
                .map_err(|_| ScanErrorKind::Unavailable)?;
            std::fs::canonicalize(binary).map_err(io_failure)
        })
        .await?;
        let cancellation = CommandCancellation::new();
        let mut version_command = scanner_command(&binary);
        version_command.current_dir(&root).arg("--version");
        let version_output = run_command(
            cx,
            deadline,
            &cancellation,
            &mut version_command,
            progress,
            4096,
        )
        .await?;
        require_success(&version_output, ScanStage::Version)?;
        let version = parse_version(&version_output.stdout)?;

        progress.stage = ScanStage::Selection;
        let (paths, excluded) = match &request.scope {
            ScanScope::Path { path } => {
                // Preserve the caller's root spelling (for example /tmp on
                // macOS) while resolving all source bytes through root_cap.
                let scoped_path = path.strip_prefix(&request.project_root).unwrap_or(path);
                let relative = relative_path(&root, scoped_path)?;
                let directory = root_cap.try_clone().map_err(io_failure)?;
                blocking_scan_io(cx, deadline, move |source_cx| {
                    collect_rust_paths(source_cx, deadline, &directory, &relative)
                })
                .await?
            }
            ScanScope::Staged | ScanScope::Diff => {
                let mut git = blocking_scan_io(cx, deadline, |_| git_command()).await?;
                git.current_dir(&root)
                    .args([
                        "-c",
                        "core.fsmonitor=false",
                        "diff",
                        "--relative",
                        "--name-only",
                        "-z",
                        "--diff-filter=ACMR",
                        "--no-ext-diff",
                        "--no-textconv",
                    ])
                    .env("GIT_OPTIONAL_LOCKS", "0");
                match &request.scope {
                    ScanScope::Staged => {
                        git.arg("--cached");
                    }
                    ScanScope::Diff => {
                        git.arg("HEAD");
                    }
                    ScanScope::Path { .. } => unreachable!(),
                }
                git.args(["--", "."]);
                let output = run_command(
                    cx,
                    deadline,
                    &cancellation,
                    &mut git,
                    progress,
                    MAX_STDERR_BYTES,
                )
                .await?;
                require_success(&output, ScanStage::Selection)?;
                parse_git_paths(&output.stdout)?
            }
        };
        if paths.is_empty() {
            return Err(ScanErrorKind::NoQualifiedInputs);
        }
        progress.stage = ScanStage::Snapshot;
        let snapshot_parent = self.snapshot_parent.clone();
        let snapshot = blocking_scan_io(cx, deadline, move |_| {
            let mut builder = tempfile::Builder::new();
            builder.prefix("ft-code-scan-");
            // keep() is immediate: the adapter has no implicit deletion on Drop.
            Ok(match snapshot_parent {
                Some(parent) => builder.tempdir_in(parent),
                None => builder.tempdir(),
            }
            .map_err(io_failure)?
            .keep())
        })
        .await?;
        progress.snapshot = Some(snapshot.clone());
        let manifest_root_hash = root_hash.clone();
        let manifest_scope_hash = scope_hash.clone();
        let (snapshot, snapshot_cap, inputs) = blocking_scan_io(cx, deadline, move |source_cx| {
            let snapshot = std::fs::canonicalize(snapshot).map_err(io_failure)?;
            let destination = open_root_nofollow(&snapshot)?;
            let inputs = copy_snapshot(source_cx, deadline, &root_cap, &destination, &paths)?;
            let manifest = serde_json::to_vec_pretty(&serde_json::json!({
                "profile": STATIC_RUST_PROFILE,
                "root_hash": manifest_root_hash,
                "scope_hash": manifest_scope_hash,
                "inputs": inputs,
            }))
            .map_err(|_| ScanErrorKind::InvalidRequest {
                reason: "snapshot manifest is not serializable",
            })?;
            checkpoint(source_cx, deadline)?;
            write_snapshot_file(&destination, Path::new("ft-scan-inputs.json"), &manifest)?;
            seal_snapshot(&destination, &inputs)?;
            Ok((snapshot, destination, inputs))
        })
        .await?;
        progress.snapshot = Some(snapshot.clone());
        progress.stage = ScanStage::Scanner;
        let mut command = scanner_command(&binary);
        command
            .current_dir(&snapshot)
            .args([
                "--format=json",
                "--ci",
                "--only=rust",
                "--skip=12,13,14",
                "--jobs=1",
                "--no-auto-update",
            ])
            .arg(&snapshot);
        let output = run_command(
            cx,
            deadline,
            &cancellation,
            &mut command,
            progress,
            MAX_STDOUT_BYTES,
        )
        .await?;
        // UBS v5.2.42 uses exit 1 for findings. Other exits are infrastructure
        // failures even if stdout happens to contain a plausible report.
        let exit_code = output.status.code();
        if !matches!(exit_code, Some(0 | 1)) {
            return Err(ScanErrorKind::NonzeroExit {
                stage: ScanStage::Scanner,
                code: exit_code,
            });
        }
        let report = parse_report(&output.stdout, &snapshot, inputs.len())?;
        let classification = Self::classify(&report);
        if (exit_code == Some(1) && report.totals.critical == 0)
            || (exit_code == Some(0) && report.totals.critical != 0)
        {
            return Err(ScanErrorKind::NonzeroExit {
                stage: ScanStage::Scanner,
                code: exit_code,
            });
        }
        checkpoint(cx, deadline)?;
        let inputs = blocking_scan_io(cx, deadline, move |source_cx| {
            verify_snapshot(source_cx, deadline, &snapshot_cap, &inputs)?;
            Ok(inputs)
        })
        .await?;
        info!(profile = STATIC_RUST_PROFILE, %root_hash, %scope_hash, version = %version,
            exit_code, files = inputs.len(), critical = report.totals.critical,
            warning = report.totals.warning, "code scan completed");
        Ok(ScanOutcome {
            profile: STATIC_RUST_PROFILE,
            supported_language: "rust",
            excluded_checks: [
                "cargo_fmt_clippy",
                "cargo_check_test",
                "cargo_dependency_checks",
            ],
            excluded_non_rust_files: excluded,
            root_hash,
            scope_hash,
            scanner_version: version,
            scanner_exit_code: exit_code.unwrap_or_default(),
            classification,
            report,
            inputs,
            retained_snapshot: snapshot,
            diagnostics: Vec::new(),
        })
    }

    /// Classify the scan result into a severity tier.
    pub fn classify(report: &ScanReport) -> ScanClassification {
        if report.totals.critical > 0 {
            ScanClassification::Critical
        } else if report.totals.warning > 100 {
            ScanClassification::HighWarning
        } else if report.totals.warning > 0 {
            ScanClassification::Warning
        } else if report.totals.info > 0 {
            ScanClassification::Info
        } else {
            ScanClassification::Clean
        }
    }
}

impl Default for CodeScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// Overall scan classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanClassification {
    /// No findings.
    Clean,
    /// Informational findings only; no warnings or critical findings.
    Info,
    /// Some warnings, no critical.
    Warning,
    /// Many warnings (> 100).
    HighWarning,
    /// Critical findings present.
    Critical,
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn checkpoint(cx: &Cx, deadline: Instant) -> Result<(), ScanErrorKind> {
    cx.checkpoint().map_err(|_| ScanErrorKind::Cancelled)?;
    if Instant::now() >= deadline {
        return Err(ScanErrorKind::Timeout);
    }
    Ok(())
}

fn io_failure(error: std::io::Error) -> ScanErrorKind {
    ScanErrorKind::Io {
        kind: format!("{:?}", error.kind()),
    }
}

/// Filesystem calls run off the async executor. The join is deliberately
/// settled: cancellation cannot leave a retained snapshot writer running
/// after this function returns. Syscalls themselves are not preemptible; the
/// byte/entry bounds and original deadline are checked between operations.
async fn blocking_scan_io<T, F>(cx: &Cx, deadline: Instant, work: F) -> Result<T, ScanErrorKind>
where
    T: Send + 'static,
    F: FnOnce(&Cx) -> Result<T, ScanErrorKind> + Send + 'static,
{
    checkpoint(cx, deadline)?;
    let source_cx = cx.clone();
    let source_mask = crate::cx::effective_cap_mask(cx);
    crate::runtime_async::spawn_blocking(move || {
        let _context = Cx::set_current(Some(source_cx.clone()));
        let _capabilities = Cx::push_restriction(source_mask);
        checkpoint(&source_cx, deadline)?;
        work(&source_cx)
    })
    .await
    .map_err(|_| ScanErrorKind::Io {
        kind: "scan filesystem worker failed".to_string(),
    })?
}

fn scanner_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env("UBS_SKIP_RUST_BUILD", "1")
        .env("UBS_SKIP_CATEGORIES", "12,13,14")
        .env("UBS_NO_AUTO_UPDATE", "1")
        .env("UBS_ENABLE_AUTO_UPDATE", "0")
        .env("UBS_OUTPUT_FORMAT", "text")
        .env("TOON_DEFAULT_FORMAT", "text")
        .env("UBS_MAX_DIR_SIZE_MB", "16")
        .env("UBS_SKIP_SIZE_CHECK", "0")
        .env("UBS_REFUSE_HOME_ROOT", "1")
        .env("JOBS", "1")
        .env("NO_COLOR", "1")
        .env_remove("UBS_PROFILE")
        .env_remove("BASH_ENV")
        .env_remove("ENV");
    command
}

fn git_command() -> Result<Command, ScanErrorKind> {
    let binary = SubprocessBridge::<serde_json::Value>::new("git")
        .with_search_paths(Vec::<PathBuf>::new())
        .resolve_binary()
        .map_err(|_| ScanErrorKind::Unavailable)?;
    // Resolve relative PATH entries before changing into the requested project.
    let mut command = Command::new(std::fs::canonicalize(binary).map_err(io_failure)?);
    command.env("GIT_CONFIG_NOSYSTEM", "1").env(
        "GIT_CONFIG_GLOBAL",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG",
        "BASH_ENV",
        "ENV",
    ] {
        command.env_remove(key);
    }
    Ok(command)
}

async fn run_command(
    cx: &Cx,
    deadline: Instant,
    cancellation: &CommandCancellation,
    command: &mut Command,
    progress: &mut ScanProgress,
    stdout_limit: usize,
) -> Result<std::process::Output, ScanErrorKind> {
    checkpoint(cx, deadline)?;
    command
        .kill_on_drop(true)
        .stdout_limit(stdout_limit)
        .stderr_limit(MAX_STDERR_BYTES);
    let report = command
        .output_with_cx_controlled(cx, deadline, cancellation)
        .await;
    let mut diagnostic = ScanProcessDiagnostic {
        stage: progress.stage,
        exit_code: None,
        spawned: report.spawned_pid.is_some(),
        supervisor_settled: report.supervisor_settled,
        stdout_bytes: None,
        stderr_bytes: None,
        stderr_excerpt: None,
    };
    if let Ok(output) = &report.output {
        diagnostic.exit_code = output.status.code();
        diagnostic.stdout_bytes = Some(output.stdout.len());
        diagnostic.stderr_bytes = Some(output.stderr.len());
        diagnostic.stderr_excerpt = Some(
            Redactor::new()
                .redact(&String::from_utf8_lossy(&output.stderr))
                .chars()
                .take(MAX_DIAGNOSTIC_CHARS)
                .collect(),
        );
    }
    info!(stage = ?diagnostic.stage, exit_code = diagnostic.exit_code,
        spawned = diagnostic.spawned, supervisor_settled = diagnostic.supervisor_settled,
        stdout_bytes = diagnostic.stdout_bytes, stderr_bytes = diagnostic.stderr_bytes,
        stdout_limit, stderr_limit = MAX_STDERR_BYTES, "code scan process settled");
    progress.diagnostics.push(diagnostic);
    if !report.supervisor_settled {
        return Err(ScanErrorKind::SupervisorUnsettled);
    }
    report.output.map_err(|error| {
        if CommandCancelled::from_io_error(&error).is_some() {
            ScanErrorKind::Cancelled
        } else if CommandTimedOut::from_io_error(&error).is_some() {
            ScanErrorKind::Timeout
        } else if let Some(exceeded) = CommandOutputLimitExceeded::from_io_error(&error) {
            ScanErrorKind::OutputTooLarge {
                stream: exceeded.stream().to_string(),
                observed: exceeded.observed(),
                limit: exceeded.limit(),
            }
        } else if CommandOutputCaptureIncomplete::from_io_error(&error).is_some() {
            ScanErrorKind::CaptureIncomplete
        } else if CommandProcessCleanupIncomplete::from_io_error(&error).is_some() {
            ScanErrorKind::CleanupIncomplete
        } else if error.kind() == std::io::ErrorKind::NotFound {
            ScanErrorKind::Unavailable
        } else if error.kind() == std::io::ErrorKind::WouldBlock {
            ScanErrorKind::Busy
        } else {
            io_failure(error)
        }
    })
}

fn require_success(output: &std::process::Output, stage: ScanStage) -> Result<(), ScanErrorKind> {
    if output.status.success() {
        Ok(())
    } else {
        Err(ScanErrorKind::NonzeroExit {
            stage,
            code: output.status.code(),
        })
    }
}

fn parse_version(bytes: &[u8]) -> Result<String, ScanErrorKind> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ScanErrorKind::VersionMismatch)?
        .trim();
    let tail = text
        .strip_prefix("UBS Meta-Runner v")
        .ok_or(ScanErrorKind::VersionMismatch)?;
    let (version, suffix) = tail.split_once(' ').unwrap_or((tail, ""));
    let valid_suffix = suffix.is_empty()
        || suffix
            .strip_prefix("(git ")
            .and_then(|sha| sha.strip_suffix(')'))
            .is_some_and(|sha| !sha.is_empty() && sha.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if version != SUPPORTED_UBS_VERSION || !valid_suffix || text.lines().count() != 1 {
        return Err(ScanErrorKind::VersionMismatch);
    }
    Ok(version.to_string())
}

fn parse_report(bytes: &[u8], snapshot: &Path, files: usize) -> Result<ScanReport, ScanErrorKind> {
    let mut report: ScanReport =
        serde_json::from_slice(bytes).map_err(|_| ScanErrorKind::MalformedOutput {
            reason: "required JSON summary fields are invalid or missing",
        })?;
    if report.project.as_deref().map(Path::new) != Some(snapshot)
        || report.scanners.len() != 1
        || report.scanners[0].language.as_deref() != Some("rust")
        || report.totals.files != files
        || files == 0
    {
        return Err(ScanErrorKind::PartialResult);
    }
    let scanner = &report.scanners[0];
    if scanner
        .extra
        .get("format")
        .and_then(serde_json::Value::as_str)
        != Some("json")
        || scanner
            .extra
            .get("project")
            .and_then(serde_json::Value::as_str)
            .map(Path::new)
            != Some(snapshot)
        || report.extra.contains_key("error")
        || scanner.extra.contains_key("error")
    {
        return Err(ScanErrorKind::PartialResult);
    }
    let summary = ScanTotals {
        files: scanner.files,
        critical: scanner.critical,
        warning: scanner.warning,
        info: scanner.info,
    };
    if report.totals != summary || report.totals.total().is_none() {
        return Err(ScanErrorKind::PartialResult);
    }
    // This API exposes validated summary counts, not arbitrary extensions or
    // raw samples. Unknown fields can contain source text or suggested commands.
    report.extra.clear();
    report.scanners[0].extra.clear();
    Ok(report)
}

fn relative_path(root: &Path, path: &Path) -> Result<PathBuf, ScanErrorKind> {
    let relative = if path.is_absolute() {
        path.strip_prefix(root)
            .map_err(|_| ScanErrorKind::PathEscape)?
    } else {
        path
    };
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::CurDir => {}
            _ => return Err(ScanErrorKind::PathEscape),
        }
    }
    if normalized.to_str().is_none() {
        return Err(ScanErrorKind::InvalidRequest {
            reason: "selected paths must be UTF-8",
        });
    }
    if normalized.components().count() > 32 || normalized.as_os_str().len() > 4096 {
        return Err(ScanErrorKind::ScopeTooLarge);
    }
    Ok(normalized)
}

fn open_root_nofollow(path: &Path) -> Result<Dir, ScanErrorKind> {
    if !path.is_absolute() || path.components().count() > 64 {
        return Err(ScanErrorKind::InvalidRequest {
            reason: "invalid absolute directory root",
        });
    }
    let mut components = path.components();
    let mut anchor = PathBuf::new();
    for component in components.by_ref() {
        anchor.push(component.as_os_str());
        if matches!(component, Component::RootDir) {
            break;
        }
    }
    let mut directory =
        Dir::open_ambient_dir(anchor, cap_std::ambient_authority()).map_err(io_failure)?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(ScanErrorKind::PathEscape);
        };
        directory = directory.open_dir_nofollow(name).map_err(io_failure)?;
    }
    Ok(directory)
}

fn open_descendant_dir(root: &Dir, path: &Path) -> Result<Dir, ScanErrorKind> {
    let mut directory = root.try_clone().map_err(io_failure)?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(ScanErrorKind::PathEscape);
        };
        if directory
            .symlink_metadata(name)
            .map_err(io_failure)?
            .file_type()
            .is_symlink()
        {
            return Err(ScanErrorKind::PathEscape);
        }
        directory = directory.open_dir_nofollow(name).map_err(io_failure)?;
    }
    Ok(directory)
}

fn collect_rust_paths(
    cx: &Cx,
    deadline: Instant,
    root: &Dir,
    selected: &Path,
) -> Result<(BTreeSet<PathBuf>, usize), ScanErrorKind> {
    let parent = open_descendant_dir(root, selected.parent().unwrap_or_else(|| Path::new("")))?;
    let leaf = selected
        .file_name()
        .map(Path::new)
        .unwrap_or_else(|| Path::new("."));
    let metadata = parent.symlink_metadata(leaf).map_err(io_failure)?;
    if metadata.file_type().is_symlink() {
        return Err(ScanErrorKind::PathEscape);
    }
    if metadata.is_file() {
        if selected
            .extension()
            .is_some_and(|extension| extension == "rs")
        {
            return Ok((BTreeSet::from([selected.to_path_buf()]), 0));
        }
        return Err(ScanErrorKind::NoQualifiedInputs);
    }
    if !metadata.is_dir() {
        return Err(ScanErrorKind::UnsupportedEntry);
    }
    // Queue names, not open directories: a wide source tree must not consume
    // thousands of process-wide descriptors before the entry cap is reached.
    // Every queued directory is reopened through the pinned root with nofollow.
    let mut pending = vec![selected.to_path_buf()];
    let mut paths = BTreeSet::new();
    let mut excluded = 0;
    let mut visited = 0;
    while let Some(relative) = pending.pop() {
        checkpoint(cx, deadline)?;
        let directory = open_descendant_dir(root, &relative)?;
        for entry in directory.entries().map_err(io_failure)? {
            checkpoint(cx, deadline)?;
            visited += 1;
            if visited > MAX_SCAN_ENTRIES {
                return Err(ScanErrorKind::ScopeTooLarge);
            }
            let entry = entry.map_err(io_failure)?;
            let name = entry.file_name();
            let path = relative_path(Path::new(""), &relative.join(&name))?;
            let metadata = directory.symlink_metadata(&name).map_err(io_failure)?;
            if metadata.file_type().is_symlink() {
                return Err(ScanErrorKind::PathEscape);
            }
            if metadata.is_dir() {
                // Git object data is never a source input. Other directories
                // remain explicit inputs and count against the traversal bound.
                if name != ".git" {
                    pending.push(path);
                }
            } else if metadata.is_file() {
                if path.extension().is_some_and(|extension| extension == "rs") {
                    paths.insert(path);
                    if paths.len() > MAX_SCAN_FILES {
                        return Err(ScanErrorKind::ScopeTooLarge);
                    }
                } else {
                    excluded += 1;
                }
            } else {
                return Err(ScanErrorKind::UnsupportedEntry);
            }
        }
    }
    Ok((paths, excluded))
}

fn parse_git_paths(bytes: &[u8]) -> Result<(BTreeSet<PathBuf>, usize), ScanErrorKind> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(ScanErrorKind::MalformedOutput {
            reason: "Git selection lacks NUL termination",
        });
    }
    let mut paths = BTreeSet::new();
    let mut excluded = 0;
    if bytes.is_empty() {
        return Ok((paths, excluded));
    }
    for (index, raw) in bytes[..bytes.len() - 1]
        .split(|byte| *byte == 0)
        .enumerate()
    {
        if index >= MAX_SCAN_ENTRIES {
            return Err(ScanErrorKind::ScopeTooLarge);
        }
        let text = std::str::from_utf8(raw).map_err(|_| ScanErrorKind::InvalidRequest {
            reason: "Git path is not UTF-8",
        })?;
        if text.is_empty() || Path::new(text).is_absolute() {
            return Err(ScanErrorKind::PathEscape);
        }
        let path = relative_path(Path::new(""), Path::new(text))?;
        if path.extension().is_some_and(|extension| extension == "rs") {
            paths.insert(path);
            if paths.len() > MAX_SCAN_FILES {
                return Err(ScanErrorKind::ScopeTooLarge);
            }
        } else {
            excluded += 1;
        }
    }
    Ok((paths, excluded))
}

fn read_source(
    cx: &Cx,
    deadline: Instant,
    root: &Dir,
    path: &Path,
) -> Result<Vec<u8>, ScanErrorKind> {
    checkpoint(cx, deadline)?;
    let parent = open_descendant_dir(root, path.parent().unwrap_or_else(|| Path::new("")))?;
    let leaf = path.file_name().ok_or(ScanErrorKind::UnsupportedEntry)?;
    let metadata = parent.symlink_metadata(leaf).map_err(io_failure)?;
    if metadata.file_type().is_symlink() {
        return Err(ScanErrorKind::PathEscape);
    }
    if !metadata.is_file() {
        return Err(ScanErrorKind::UnsupportedEntry);
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.nonblock(true);
    let mut file = parent.open_with(leaf, &options).map_err(io_failure)?;
    if !file.metadata().map_err(io_failure)?.is_file() {
        return Err(ScanErrorKind::UnsupportedEntry);
    }
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        checkpoint(cx, deadline)?;
        let count = file.read(&mut buffer).map_err(io_failure)?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > MAX_FILE_BYTES {
            return Err(ScanErrorKind::ScopeTooLarge);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn copy_snapshot(
    cx: &Cx,
    deadline: Instant,
    source: &Dir,
    destination: &Dir,
    paths: &BTreeSet<PathBuf>,
) -> Result<Vec<ScanInput>, ScanErrorKind> {
    let mut inputs = Vec::new();
    let mut total = 0;
    for path in paths {
        let bytes = read_source(cx, deadline, source, path)?;
        total += bytes.len();
        if total > MAX_SNAPSHOT_BYTES {
            return Err(ScanErrorKind::ScopeTooLarge);
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            destination.create_dir_all(parent).map_err(io_failure)?;
        }
        write_snapshot_file(destination, path, &bytes)?;
        inputs.push(ScanInput {
            origin_path: path.clone(),
            bytes: bytes.len(),
            sha256: digest(&bytes),
        });
    }
    Ok(inputs)
}

fn write_snapshot_file(directory: &Dir, path: &Path, bytes: &[u8]) -> Result<(), ScanErrorKind> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = directory.open_with(path, &options).map_err(io_failure)?;
    file.write_all(bytes).map_err(io_failure)?;
    file.sync_all().map_err(io_failure)?;
    let mut permissions = file.metadata().map_err(io_failure)?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions).map_err(io_failure)
}

fn seal_snapshot(root: &Dir, inputs: &[ScanInput]) -> Result<(), ScanErrorKind> {
    let mut directories = BTreeSet::from([PathBuf::new()]);
    for input in inputs {
        let mut parent = input.origin_path.parent();
        while let Some(path) = parent {
            directories.insert(path.to_path_buf());
            parent = path.parent();
        }
    }
    for path in directories.into_iter().rev() {
        let directory = open_descendant_dir(root, &path)?;
        let mut permissions = directory.dir_metadata().map_err(io_failure)?.permissions();
        permissions.set_readonly(true);
        directory
            .set_permissions(".", permissions)
            .map_err(io_failure)?;
    }
    Ok(())
}

fn verify_snapshot(
    cx: &Cx,
    deadline: Instant,
    root: &Dir,
    inputs: &[ScanInput],
) -> Result<(), ScanErrorKind> {
    for input in inputs {
        let bytes = read_source(cx, deadline, root, &input.origin_path)?;
        if bytes.len() != input.bytes || digest(&bytes) != input.sha256 {
            return Err(ScanErrorKind::PartialResult);
        }
    }
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn report_json(project: &Path) -> serde_json::Value {
        serde_json::json!({
            "project": project,
            "scanners": [{"project": project, "format": "json", "language": "rust",
                "files": 1, "critical": 0, "warning": 0, "info": 0}],
            "totals": {"files": 1, "critical": 0, "warning": 0, "info": 0}
        })
    }

    #[test]
    fn scanner_schema_rejects_empty_malformed_partial_and_overflow_reports() {
        let project = Path::new("/owned/snapshot");
        let valid = report_json(project);
        assert!(parse_report(&serde_json::to_vec(&valid).unwrap(), project, 1).is_ok());
        for malformed in [
            b"{}".as_slice(),
            br#"{"project":"unterminated"#,
            br#"{"sample":"call("unescaped")"}"#,
        ] {
            assert!(matches!(
                parse_report(malformed, project, 1),
                Err(ScanErrorKind::MalformedOutput { .. })
            ));
        }
        let mut cases = Vec::new();
        let mut wrong_root = valid.clone();
        wrong_root["project"] = serde_json::json!("/outside");
        cases.push(wrong_root);
        let mut wrong_count = valid.clone();
        wrong_count["totals"]["files"] = serde_json::json!(2);
        cases.push(wrong_count);
        let mut scraped_fallback = valid.clone();
        scraped_fallback["scanners"][0]
            .as_object_mut()
            .unwrap()
            .remove("format");
        cases.push(scraped_fallback);
        let mut partial = valid.clone();
        partial["scanners"] = serde_json::json!([]);
        cases.push(partial);
        let mut overflow = valid.clone();
        for field in ["critical", "warning", "info"] {
            overflow["totals"][field] = serde_json::json!(usize::MAX);
            overflow["scanners"][0][field] = serde_json::json!(usize::MAX);
        }
        cases.push(overflow);
        for case in cases {
            assert!(matches!(
                parse_report(&serde_json::to_vec(&case).unwrap(), project, 1),
                Err(ScanErrorKind::PartialResult)
            ));
        }
        let totals = ScanTotals {
            critical: usize::MAX,
            warning: 1,
            ..ScanTotals::default()
        };
        assert_eq!(totals.total(), None);
    }

    #[test]
    fn scanner_summary_retains_info_and_excludes_untrusted_extensions() {
        let project = Path::new("/owned/snapshot");
        let mut value = report_json(project);
        value["totals"]["info"] = serde_json::json!(2);
        value["scanners"][0]["info"] = serde_json::json!(2);
        value["suggestion"] = serde_json::json!("execute-this-command");
        value["scanners"][0]["sample"] = serde_json::json!("private source text");
        let report = parse_report(&serde_json::to_vec(&value).unwrap(), project, 1).unwrap();
        assert_eq!(CodeScanner::classify(&report), ScanClassification::Info);
        assert!(report.extra.is_empty() && report.scanners[0].extra.is_empty());
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(!encoded.contains("private source") && !encoded.contains("execute-this"));
    }

    #[test]
    fn scanner_git_selection_is_nul_framed_and_cannot_escape() {
        let (paths, excluded) =
            parse_git_paths(b"src/a.rs\0src/line\nname.rs\0README.md\0").unwrap();
        assert_eq!(
            paths,
            BTreeSet::from([
                PathBuf::from("src/a.rs"),
                PathBuf::from("src/line\nname.rs"),
            ])
        );
        assert_eq!(excluded, 1);
        for bad in [b"../outside.rs\0".as_slice(), b"/outside.rs\0", b"\0"] {
            assert!(matches!(
                parse_git_paths(bad),
                Err(ScanErrorKind::PathEscape)
            ));
        }
        assert!(matches!(
            parse_git_paths(b"src/a.rs"),
            Err(ScanErrorKind::MalformedOutput { .. })
        ));
        assert!(parse_git_paths(b"").unwrap().0.is_empty());
        assert!(matches!(
            relative_path(Path::new("/owned"), Path::new("/outside/a.rs")),
            Err(ScanErrorKind::PathEscape)
        ));
    }

    #[test]
    fn scanner_version_requires_the_qualified_contract() {
        assert_eq!(
            parse_version(b"UBS Meta-Runner v5.2.42 (git abc123)\n").unwrap(),
            "5.2.42"
        );
        for text in [
            "5.2.42",
            "UBS Meta-Runner v9.0.0",
            "UBS Meta-Runner v5.2.42\nnoise",
            "UBS Meta-Runner v5.2.42 arbitrary suffix",
            "UBS Meta-Runner v5.2.42 (git nope)",
        ] {
            assert_eq!(
                parse_version(text.as_bytes()),
                Err(ScanErrorKind::VersionMismatch)
            );
        }
    }

    #[cfg(unix)]
    mod process_tests {
        use super::*;
        use crate::runtime_async::CompatRuntime;
        use std::os::unix::fs::{PermissionsExt, symlink};

        struct Fixture {
            base: PathBuf,
            root: PathBuf,
            scanner: CodeScanner,
        }

        impl Fixture {
            fn new(body: &str, version: &str) -> Self {
                let base = tempfile::Builder::new()
                    .prefix("ft-scanner-test-")
                    .tempdir_in("/tmp")
                    .unwrap()
                    .keep();
                let base = std::fs::canonicalize(base).unwrap();
                let root = base.join("project");
                std::fs::create_dir_all(root.join("src")).unwrap();
                std::fs::create_dir(base.join("retained")).unwrap();
                std::fs::write(
                    root.join("src/selected.rs"),
                    b"pub const SELECTED: u8 = 7;\n",
                )
                .unwrap();
                std::fs::write(root.join("other.rs"), b"pub const UNSELECTED: u8 = 99;\n").unwrap();
                let binary = base.join("scanner.sh");
                // This is an owned command fixture, not UBS evidence. It
                // records real arguments and reads the real retained snapshot.
                let script = format!(
                    r#"#!/bin/sh
set -eu
base=${{0%/*}}
printf 'called\n' >> "$base/calls"
if [ "$1" = --version ]; then
  printf '%s\n' 'UBS Meta-Runner v{version}'
  exit 0
fi
for snapshot do :; done
printf '%s\0' "$@" > "$base/argv"
printf '%s\n' "$$" > "$base/scan.pid"
test "$UBS_SKIP_RUST_BUILD" = 1
test "$UBS_SKIP_CATEGORIES" = 12,13,14
test "$UBS_NO_AUTO_UPDATE" = 1
test "$JOBS" = 1
report() {{
  printf '{{"project":"%s","scanners":[{{"project":"%s","format":"json","language":"rust","files":1,"critical":%s,"warning":0,"info":0}}],"totals":{{"files":1,"critical":%s,"warning":0,"info":0}}}}\n' "$snapshot" "$snapshot" "$1" "$1"
}}
{body}
"#
                );
                std::fs::write(&binary, script).unwrap();
                std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
                let scanner = CodeScanner::new()
                    .with_binary(&binary)
                    .unwrap()
                    .with_snapshot_parent(base.join("retained"));
                Self {
                    base,
                    root,
                    scanner,
                }
            }

            fn request(&self) -> ScanRequest {
                ScanRequest {
                    project_root: self.root.clone(),
                    scope: ScanScope::Path {
                        path: PathBuf::from("src"),
                    },
                    timeout_ms: 5000,
                }
            }

            fn git(&self, args: &[&str]) {
                let output = git_command()
                    .unwrap()
                    .current_dir(&self.root)
                    .args([
                        "-c",
                        "core.hooksPath=/dev/null",
                        "-c",
                        "commit.gpgsign=false",
                        "-c",
                        "user.name=ScannerFixture",
                        "-c",
                        "user.email=scanner@example.invalid",
                    ])
                    .args(args)
                    .stdout_limit(MAX_STDERR_BYTES)
                    .stderr_limit(MAX_STDERR_BYTES)
                    .output_blocking(Duration::from_secs(5))
                    .unwrap();
                assert!(
                    output.status.success(),
                    "owned fixture Git failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        fn run(future: impl std::future::Future<Output = ()>) {
            let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(future);
        }

        #[test]
        fn actual_command_receives_only_selected_snapshot_and_returns_findings() {
            run(async {
                let fixture = Fixture::new(
                    r#"
test -f "$snapshot/src/selected.rs"
test ! -e "$snapshot/other.rs"
cat "$snapshot/src/selected.rs" > "$base/received.rs"
printf 'token=abcdefghijklmnopqrstuvwxyz1234567890\n' >&2
report 3
exit 1"#,
                    SUPPORTED_UBS_VERSION,
                );
                let result = fixture
                    .scanner
                    .scan(
                        &crate::cx::for_testing(),
                        &mut PolicyEngine::permissive(),
                        ActorKind::Human,
                        &fixture.request(),
                        Instant::now() + Duration::from_secs(5),
                    )
                    .await
                    .unwrap();
                assert_eq!(result.classification, ScanClassification::Critical);
                assert_eq!(result.scanner_exit_code, 1);
                assert_eq!(result.report.totals.critical, 3);
                assert_eq!(result.inputs.len(), 1);
                assert_eq!(result.inputs[0].origin_path, Path::new("src/selected.rs"));
                let original = std::fs::read(fixture.root.join("src/selected.rs")).unwrap();
                assert_eq!(
                    std::fs::read(fixture.base.join("received.rs")).unwrap(),
                    original
                );
                assert_eq!(result.inputs[0].sha256, digest(&original));
                assert!(
                    result
                        .retained_snapshot
                        .join("ft-scan-inputs.json")
                        .is_file()
                );
                assert!(
                    std::fs::metadata(&result.retained_snapshot)
                        .unwrap()
                        .permissions()
                        .readonly()
                );
                assert!(
                    std::fs::metadata(result.retained_snapshot.join("src/selected.rs"))
                        .unwrap()
                        .permissions()
                        .readonly()
                );
                assert!(
                    result
                        .diagnostics
                        .iter()
                        .all(|d| d.spawned && d.supervisor_settled)
                );
                assert_eq!(result.diagnostics.len(), 2);
                let stderr = result.diagnostics[1].stderr_excerpt.as_deref().unwrap();
                assert!(!stderr.contains("abcdefghijklmnopqrstuvwxyz1234567890"));
                let args = std::fs::read(fixture.base.join("argv")).unwrap();
                let args: Vec<_> = args
                    .split(|byte| *byte == 0)
                    .filter(|arg| !arg.is_empty())
                    .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
                    .collect();
                assert_eq!(
                    &args[..6],
                    [
                        "--format=json",
                        "--ci",
                        "--only=rust",
                        "--skip=12,13,14",
                        "--jobs=1",
                        "--no-auto-update"
                    ]
                );
                assert_eq!(Path::new(&args[6]), result.retained_snapshot);
            });
        }

        #[test]
        fn actual_git_staged_and_diff_scopes_select_current_bytes_without_extra_files() {
            run(async {
                for scope in [ScanScope::Staged, ScanScope::Diff] {
                    let fixture = Fixture::new(
                        r#"
test -f "$snapshot/src/selected.rs"
test ! -e "$snapshot/other.rs"
test ! -e "$snapshot/untracked.rs"
cat "$snapshot/src/selected.rs" > "$base/received.rs"
report 0"#,
                        SUPPORTED_UBS_VERSION,
                    );
                    fixture.git(&["init", "--template=", "--initial-branch=main"]);
                    fixture.git(&["add", "--", "src/selected.rs", "other.rs"]);
                    fixture.git(&["commit", "-m", "owned scanner baseline"]);
                    std::fs::write(
                        fixture.root.join("src/selected.rs"),
                        b"pub const SELECTED: u8 = 8;\n",
                    )
                    .unwrap();
                    if scope == ScanScope::Staged {
                        fixture.git(&["add", "--", "src/selected.rs"]);
                        // Staged selects index path names, while the declared
                        // profile scans the current working-tree bytes.
                        std::fs::write(
                            fixture.root.join("src/selected.rs"),
                            b"pub const SELECTED: u8 = 9;\n",
                        )
                        .unwrap();
                        std::fs::write(
                            fixture.root.join("other.rs"),
                            b"pub const UNSTAGED: u8 = 1;\n",
                        )
                        .unwrap();
                    }
                    std::fs::write(fixture.root.join("untracked.rs"), b"UNTRACKED_SENTINEL")
                        .unwrap();
                    let mut request = fixture.request();
                    request.scope = scope;
                    let result = fixture
                        .scanner
                        .scan(
                            &crate::cx::for_testing(),
                            &mut PolicyEngine::permissive(),
                            ActorKind::Human,
                            &request,
                            Instant::now() + Duration::from_secs(5),
                        )
                        .await
                        .unwrap();
                    assert_eq!(result.classification, ScanClassification::Clean);
                    assert_eq!(result.inputs.len(), 1);
                    assert_eq!(result.inputs[0].origin_path, Path::new("src/selected.rs"));
                    assert_eq!(
                        std::fs::read(fixture.base.join("received.rs")).unwrap(),
                        std::fs::read(fixture.root.join("src/selected.rs")).unwrap()
                    );
                    assert_eq!(result.diagnostics.len(), 3);
                    assert_eq!(result.diagnostics[1].stage, ScanStage::Selection);
                    assert!(
                        result
                            .diagnostics
                            .iter()
                            .all(|d| d.spawned && d.supervisor_settled)
                    );
                }
            });
        }

        #[test]
        fn unqualified_empty_and_oversized_scopes_refuse_before_scanner_execution() {
            run(async {
                for case in ["non_rust", "empty", "oversized_file", "too_many_files"] {
                    let fixture = Fixture::new("report 0", SUPPORTED_UBS_VERSION);
                    let mut request = fixture.request();
                    match case {
                        "non_rust" => {
                            std::fs::write(
                                fixture.root.join("other.py"),
                                b"print('not qualified')\n",
                            )
                            .unwrap();
                            request.scope = ScanScope::Path {
                                path: PathBuf::from("other.py"),
                            };
                        }
                        "empty" => {
                            std::fs::create_dir(fixture.root.join("empty")).unwrap();
                            request.scope = ScanScope::Path {
                                path: PathBuf::from("empty"),
                            };
                        }
                        "oversized_file" => {
                            std::fs::write(
                                fixture.root.join("src/selected.rs"),
                                vec![b'x'; MAX_FILE_BYTES + 1],
                            )
                            .unwrap();
                        }
                        "too_many_files" => {
                            for index in 0..MAX_SCAN_FILES {
                                std::fs::write(
                                    fixture.root.join(format!("src/extra-{index}.rs")),
                                    b"// additional selected file\n",
                                )
                                .unwrap();
                            }
                        }
                        _ => unreachable!(),
                    }
                    let error = fixture
                        .scanner
                        .scan(
                            &crate::cx::for_testing(),
                            &mut PolicyEngine::permissive(),
                            ActorKind::Human,
                            &request,
                            Instant::now() + Duration::from_secs(5),
                        )
                        .await
                        .unwrap_err();
                    assert_eq!(
                        error.kind,
                        if matches!(case, "non_rust" | "empty") {
                            ScanErrorKind::NoQualifiedInputs
                        } else {
                            ScanErrorKind::ScopeTooLarge
                        }
                    );
                    assert_eq!(error.diagnostics.len(), 1);
                    assert!(!fixture.base.join("scan.pid").exists());
                    assert_eq!(error.retained_snapshot.is_some(), case == "oversized_file");
                }
            });
        }

        #[test]
        fn immutable_snapshot_survives_project_mutation_and_detects_changed_captured_bytes() {
            let fixture = Fixture::new("report 0", SUPPORTED_UBS_VERSION);
            let cx = crate::cx::for_testing();
            let deadline = Instant::now() + Duration::from_secs(5);
            let source = open_root_nofollow(&fixture.root).unwrap();
            let destination = open_root_nofollow(&fixture.base.join("retained")).unwrap();
            let paths = BTreeSet::from([PathBuf::from("src/selected.rs")]);
            let inputs = copy_snapshot(&cx, deadline, &source, &destination, &paths).unwrap();
            seal_snapshot(&destination, &inputs).unwrap();
            std::fs::write(
                fixture.root.join("src/selected.rs"),
                b"CHANGED_PROJECT_BYTES",
            )
            .unwrap();
            verify_snapshot(&cx, deadline, &destination, &inputs).unwrap();
            let captured = fixture.base.join("retained/src/selected.rs");
            assert_eq!(
                std::fs::read(&captured).unwrap(),
                b"pub const SELECTED: u8 = 7;\n"
            );
            // A deliberate owner-level mutation of this private test artifact
            // must invalidate the result; source changes alone do not.
            std::fs::set_permissions(&captured, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::write(&captured, b"TAMPERED_SNAPSHOT_BYTES").unwrap();
            assert_eq!(
                verify_snapshot(&cx, deadline, &destination, &inputs),
                Err(ScanErrorKind::PartialResult)
            );
        }

        #[test]
        fn unavailable_is_typed_for_every_requested_scope() {
            run(async {
                let fixture = Fixture::new("report 0", SUPPORTED_UBS_VERSION);
                let scanner = fixture
                    .scanner
                    .clone()
                    .with_binary(&fixture.base.join("missing"))
                    .unwrap();
                for scope in [
                    ScanScope::Path {
                        path: PathBuf::from("src"),
                    },
                    ScanScope::Staged,
                    ScanScope::Diff,
                ] {
                    let mut request = fixture.request();
                    request.scope = scope;
                    let error = scanner
                        .scan(
                            &crate::cx::for_testing(),
                            &mut PolicyEngine::permissive(),
                            ActorKind::Human,
                            &request,
                            Instant::now() + Duration::from_secs(5),
                        )
                        .await
                        .unwrap_err();
                    assert_eq!(error.kind, ScanErrorKind::Unavailable);
                    assert!(error.diagnostics.is_empty() && error.retained_snapshot.is_none());
                }
                assert!(!fixture.base.join("calls").exists());
            });
        }

        #[test]
        fn denied_and_approval_required_policy_spawn_no_process() {
            run(async {
                let fixture = Fixture::new("report 0", SUPPORTED_UBS_VERSION);
                for (decision, expected) in [
                    ("deny", ScanErrorKind::PolicyDenied),
                    ("require_approval", ScanErrorKind::ApprovalRequired),
                ] {
                    let rules = serde_json::from_value(serde_json::json!({
                        "rules": [{"id":"scanner-test", "decision":decision,
                            "match_on":{"actions":["exec_command"]}}]
                    }))
                    .unwrap();
                    let mut policy = PolicyEngine::permissive().with_policy_rules(rules);
                    let error = fixture
                        .scanner
                        .scan(
                            &crate::cx::for_testing(),
                            &mut policy,
                            ActorKind::Human,
                            &fixture.request(),
                            Instant::now() + Duration::from_secs(5),
                        )
                        .await
                        .unwrap_err();
                    assert_eq!(error.kind, expected);
                    assert!(error.diagnostics.is_empty() && error.retained_snapshot.is_none());
                }
                assert!(!fixture.base.join("calls").exists());
            });
        }

        #[test]
        fn bad_version_nonzero_malformed_and_partial_never_become_clean() {
            run(async {
                for (body, version, expected) in [
                    ("report 0", "9.0.0", ScanErrorKind::VersionMismatch),
                    (
                        "report 0; exit 2",
                        SUPPORTED_UBS_VERSION,
                        ScanErrorKind::NonzeroExit {
                            stage: ScanStage::Scanner,
                            code: Some(2),
                        },
                    ),
                    (
                        "report 0; exit 1",
                        SUPPORTED_UBS_VERSION,
                        ScanErrorKind::NonzeroExit {
                            stage: ScanStage::Scanner,
                            code: Some(1),
                        },
                    ),
                    (
                        "printf '{}'",
                        SUPPORTED_UBS_VERSION,
                        ScanErrorKind::MalformedOutput {
                            reason: "required JSON summary fields are invalid or missing",
                        },
                    ),
                    (
                        r#"report 0 | sed 's/"files":1/"files":2/g'"#,
                        SUPPORTED_UBS_VERSION,
                        ScanErrorKind::PartialResult,
                    ),
                ] {
                    let fixture = Fixture::new(body, version);
                    let error = fixture
                        .scanner
                        .scan(
                            &crate::cx::for_testing(),
                            &mut PolicyEngine::permissive(),
                            ActorKind::Human,
                            &fixture.request(),
                            Instant::now() + Duration::from_secs(5),
                        )
                        .await
                        .unwrap_err();
                    assert_eq!(error.kind, expected);
                    assert!(error.diagnostics.iter().all(|d| d.supervisor_settled));
                    if version == "9.0.0" {
                        assert_eq!(error.diagnostics.len(), 1);
                        assert!(error.retained_snapshot.is_none());
                    } else {
                        assert!(
                            error
                                .retained_snapshot
                                .unwrap()
                                .join("src/selected.rs")
                                .is_file()
                        );
                    }
                }
            });
        }

        #[test]
        fn cancelled_request_never_spawns_and_running_scan_settles() {
            run(async {
                let fixture = Fixture::new("exec sleep 10", SUPPORTED_UBS_VERSION);
                let cx = crate::cx::for_testing();
                cx.cancel_with(crate::outcome::CancelKind::User, Some("scanner pre-cancel"));
                let error = fixture
                    .scanner
                    .scan(
                        &cx,
                        &mut PolicyEngine::permissive(),
                        ActorKind::Human,
                        &fixture.request(),
                        Instant::now() + Duration::from_secs(5),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.kind, ScanErrorKind::Cancelled);
                assert!(!fixture.base.join("calls").exists());
                let cx = crate::cx::for_testing();
                let canceller = cx.clone();
                let marker = fixture.base.join("scan.pid");
                let trigger = std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(4);
                    while !marker.is_file() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    assert!(
                        marker.is_file(),
                        "scan process must start before cancellation"
                    );
                    canceller.cancel_with(
                        crate::outcome::CancelKind::User,
                        Some("scanner in-flight cancel"),
                    );
                });
                let error = fixture
                    .scanner
                    .scan(
                        &cx,
                        &mut PolicyEngine::permissive(),
                        ActorKind::Human,
                        &fixture.request(),
                        Instant::now() + Duration::from_secs(5),
                    )
                    .await
                    .unwrap_err();
                trigger.join().unwrap();
                assert_eq!(error.kind, ScanErrorKind::Cancelled);
                assert_eq!(error.stage, ScanStage::Scanner);
                assert!(error.diagnostics.last().unwrap().supervisor_settled);
                let pid: i64 = std::fs::read_to_string(fixture.base.join("scan.pid"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                assert!(
                    !crate::runtime_async::process::send_unix_signal_to_pid(pid, "0")
                        .unwrap()
                        .success()
                );
            });
        }

        #[test]
        fn policy_source_refuses_replacement_after_pin_without_reading_redirected_state() {
            for replacement in ["leaf", "parent", "appeared_after_absence"] {
                let fixture = Fixture::new("report 0", SUPPORTED_UBS_VERSION);
                let database = fixture.root.join(".ft/ft.db");
                if replacement != "appeared_after_absence" {
                    std::fs::create_dir(fixture.root.join(".ft")).unwrap();
                    std::fs::write(&database, b"ORIGINAL_DATABASE_SENTINEL").unwrap();
                }
                let source = ScanPolicySource::prepare(&fixture.root, &database).unwrap();
                source.verify().unwrap();
                let outside = fixture.base.join("outside.db");
                std::fs::write(&outside, b"OUTSIDE_DATABASE_SENTINEL").unwrap();
                match replacement {
                    "leaf" => {
                        std::fs::rename(&database, database.with_extension("retained")).unwrap();
                        std::os::unix::fs::symlink(&outside, &database).unwrap();
                    }
                    "parent" => {
                        std::fs::rename(
                            fixture.root.join(".ft"),
                            fixture.base.join("retained-state"),
                        )
                        .unwrap();
                        std::fs::create_dir(fixture.root.join(".ft")).unwrap();
                        std::fs::write(&database, b"REPLACEMENT_DATABASE_SENTINEL").unwrap();
                    }
                    "appeared_after_absence" => {
                        std::fs::create_dir(fixture.root.join(".ft")).unwrap();
                        std::fs::write(&database, b"NEW_DATABASE_SENTINEL").unwrap();
                    }
                    _ => unreachable!(),
                }
                let mut policy = PolicyEngine::permissive();
                let result = source.restore(
                    &crate::cx::for_testing(),
                    Instant::now() + Duration::from_secs(5),
                    &mut policy,
                );
                assert!(
                    matches!(result, Err(ScanErrorKind::Io { ref kind }) if kind == "policy_state_unavailable"),
                    "{replacement}"
                );
                assert_eq!(
                    std::fs::read(&outside).unwrap(),
                    b"OUTSIDE_DATABASE_SENTINEL"
                );
                assert!(
                    !fixture
                        .root
                        .join(".ft/ft.db.policy-kill-switch.lock")
                        .exists()
                );
            }
        }

        #[test]
        fn expired_caller_deadline_is_never_rebased_and_spawns_no_process() {
            run(async {
                let fixture = Fixture::new("report 0", SUPPORTED_UBS_VERSION);
                let error = fixture
                    .scanner
                    .scan(
                        &crate::cx::for_testing(),
                        &mut PolicyEngine::permissive(),
                        ActorKind::Human,
                        &fixture.request(),
                        Instant::now() - Duration::from_secs(1),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.kind, ScanErrorKind::Timeout);
                assert_eq!(error.stage, ScanStage::Admission);
                assert!(error.diagnostics.is_empty());
                assert!(error.retained_snapshot.is_none());
                assert!(!fixture.base.join("calls").exists());
            });
        }

        #[test]
        fn scanner_deadline_and_both_output_limits_fail_explicitly() {
            run(async {
                let fixture = Fixture::new("exec sleep 10", SUPPORTED_UBS_VERSION);
                let mut request = fixture.request();
                request.timeout_ms = 1500;
                let error = fixture
                    .scanner
                    .scan(
                        &crate::cx::for_testing(),
                        &mut PolicyEngine::permissive(),
                        ActorKind::Human,
                        &request,
                        Instant::now() + Duration::from_secs(5),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.kind, ScanErrorKind::Timeout);
                assert_eq!(error.stage, ScanStage::Scanner);
                assert!(error.diagnostics.last().unwrap().supervisor_settled);
                for (redirect, stream) in [("", "stdout"), (">&2", "stderr")] {
                    let body = format!(
                        "i=0; while [ \"$i\" -lt 20000 ]; do printf '%0100d' 0 {redirect}; i=$((i+1)); done"
                    );
                    let fixture = Fixture::new(&body, SUPPORTED_UBS_VERSION);
                    let error = fixture
                        .scanner
                        .scan(
                            &crate::cx::for_testing(),
                            &mut PolicyEngine::permissive(),
                            ActorKind::Human,
                            &fixture.request(),
                            Instant::now() + Duration::from_secs(5),
                        )
                        .await
                        .unwrap_err();
                    assert!(
                        matches!(&error.kind, ScanErrorKind::OutputTooLarge { stream: actual, .. } if actual == stream)
                    );
                    assert!(error.diagnostics.last().unwrap().supervisor_settled);
                }
            });
        }

        #[test]
        fn snapshot_capability_rejects_replaced_ancestor_and_retains_original_bytes() {
            let fixture = Fixture::new("report 0", SUPPORTED_UBS_VERSION);
            let cx = crate::cx::for_testing();
            let deadline = Instant::now() + Duration::from_secs(5);
            let source = open_root_nofollow(&fixture.root).unwrap();
            let (paths, _) = collect_rust_paths(&cx, deadline, &source, Path::new("src")).unwrap();
            let outside = fixture.base.join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("selected.rs"), b"OUTSIDE_SENTINEL").unwrap();
            let saved = fixture.root.join("saved-src");
            assert!(!saved.exists());
            std::fs::rename(fixture.root.join("src"), &saved).unwrap();
            symlink(&outside, fixture.root.join("src")).unwrap();
            let retained = open_root_nofollow(&fixture.base.join("retained")).unwrap();
            assert!(matches!(
                copy_snapshot(&cx, deadline, &source, &retained, &paths),
                Err(ScanErrorKind::PathEscape)
            ));
            assert!(!fixture.base.join("retained/src/selected.rs").exists());
            assert_eq!(
                std::fs::read(saved.join("selected.rs")).unwrap(),
                b"pub const SELECTED: u8 = 7;\n"
            );
            assert_eq!(
                std::fs::read(outside.join("selected.rs")).unwrap(),
                b"OUTSIDE_SENTINEL"
            );
        }

        #[test]
        fn wide_directory_scope_stays_within_owned_descriptor_budget() {
            const SELECTOR: &str = "code_scanner::tests::process_tests::wide_directory_scope_stays_within_owned_descriptor_budget";
            if std::env::var_os("FT_SCANNER_DESCRIPTOR_CHILD").as_deref()
                != Some(std::ffi::OsStr::new("1"))
            {
                let output = Command::new("/bin/sh")
                    .args([
                        "-c",
                        "ulimit -n 96 || exit 95; exec \"$1\" --exact \"$2\" --nocapture",
                        "scanner-descriptor-test",
                    ])
                    .arg(std::env::current_exe().unwrap())
                    .arg(SELECTOR)
                    .env("FT_SCANNER_DESCRIPTOR_CHILD", "1")
                    .stdout_limit(MAX_STDERR_BYTES)
                    .stderr_limit(MAX_STDERR_BYTES)
                    .output_blocking(Duration::from_secs(10))
                    .unwrap();
                assert!(
                    output.status.success(),
                    "owned descriptor child failed: stdout={}; stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let stdout = String::from_utf8(output.stdout).unwrap();
                assert!(stdout.contains(&format!("test {SELECTOR} ... ok")));
                assert!(stdout.contains("1 passed; 0 failed; 0 ignored"));
                return;
            }
            let fixture = Fixture::new("report 0", SUPPORTED_UBS_VERSION);
            for index in 0..256 {
                std::fs::create_dir(fixture.root.join(format!("wide-{index}"))).unwrap();
            }
            let cx = crate::cx::for_testing();
            let directory = open_root_nofollow(&fixture.root).unwrap();
            let (paths, excluded) = collect_rust_paths(
                &cx,
                Instant::now() + Duration::from_secs(5),
                &directory,
                Path::new(""),
            )
            .unwrap();
            assert_eq!(
                paths,
                BTreeSet::from([PathBuf::from("other.rs"), PathBuf::from("src/selected.rs")])
            );
            assert_eq!(excluded, 0);
        }
    }

    // -------------------------------------------------------------------------
    // FindingSeverity
    // -------------------------------------------------------------------------

    #[test]
    fn test_finding_severity_display() {
        assert_eq!(FindingSeverity::Info.to_string(), "info");
        assert_eq!(FindingSeverity::Warning.to_string(), "warning");
        assert_eq!(FindingSeverity::Critical.to_string(), "critical");
    }

    #[test]
    fn test_finding_severity_ord() {
        assert!(FindingSeverity::Info < FindingSeverity::Warning);
        assert!(FindingSeverity::Warning < FindingSeverity::Critical);
    }

    #[test]
    fn test_finding_severity_serde_roundtrip() {
        for sev in [
            FindingSeverity::Info,
            FindingSeverity::Warning,
            FindingSeverity::Critical,
        ] {
            let json = serde_json::to_string(&sev).unwrap();
            let back: FindingSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(back, sev);
        }
    }

    #[test]
    fn test_finding_severity_deserialize_lowercase() {
        let sev: FindingSeverity = serde_json::from_str("\"critical\"").unwrap();
        assert_eq!(sev, FindingSeverity::Critical);
    }

    // -------------------------------------------------------------------------
    // ScanFinding
    // -------------------------------------------------------------------------

    #[test]
    fn test_scan_finding_full() {
        let finding = ScanFinding {
            severity: FindingSeverity::Critical,
            category: "resource-lifecycle".to_string(),
            message: "Potential resource leak".to_string(),
            file: Some("main.rs".to_string()),
            line: Some(42),
            suggestion: Some("Add drop guard".to_string()),
            extra: HashMap::new(),
        };
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.line, Some(42));
        assert!(finding.suggestion.is_some());
    }

    #[test]
    fn test_scan_finding_minimal() {
        let finding = ScanFinding {
            severity: FindingSeverity::Info,
            category: "style".to_string(),
            message: "Consider renaming".to_string(),
            file: None,
            line: None,
            suggestion: None,
            extra: HashMap::new(),
        };
        assert!(finding.file.is_none());
        assert!(finding.line.is_none());
    }

    #[test]
    fn test_scan_finding_serde_roundtrip() {
        let finding = ScanFinding {
            severity: FindingSeverity::Warning,
            category: "security".to_string(),
            message: "Hardcoded credential".to_string(),
            file: Some("config.rs".to_string()),
            line: Some(10),
            suggestion: Some("Use env var".to_string()),
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&finding).unwrap();
        let back: ScanFinding = serde_json::from_str(&json).unwrap();
        assert_eq!(back.severity, FindingSeverity::Warning);
        assert_eq!(back.category, "security");
        assert_eq!(back.file, Some("config.rs".to_string()));
    }

    #[test]
    fn test_scan_finding_forward_compat() {
        let json = r#"{
            "severity": "info",
            "category": "test",
            "message": "msg",
            "new_field": 42
        }"#;
        let finding: ScanFinding = serde_json::from_str(json).unwrap();
        assert_eq!(finding.extra.get("new_field").unwrap(), &42);
    }

    #[test]
    fn test_scan_finding_has_suggestion() {
        let with = ScanFinding {
            severity: FindingSeverity::Warning,
            category: "perf".to_string(),
            message: "Slow path".to_string(),
            file: None,
            line: None,
            suggestion: Some("Use cache".to_string()),
            extra: HashMap::new(),
        };
        assert!(with.suggestion.is_some());

        let without = ScanFinding {
            severity: FindingSeverity::Info,
            category: "style".to_string(),
            message: "Minor".to_string(),
            file: None,
            line: None,
            suggestion: None,
            extra: HashMap::new(),
        };
        assert!(without.suggestion.is_none());
    }

    // -------------------------------------------------------------------------
    // ScannerSummary
    // -------------------------------------------------------------------------

    #[test]
    fn test_scanner_summary_serde() {
        let json = r#"{
            "language": "rust",
            "files": 250,
            "critical": 10,
            "warning": 500,
            "info": 200
        }"#;
        let summary: ScannerSummary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.language, Some("rust".to_string()));
        assert_eq!(summary.files, 250);
        assert_eq!(summary.critical, 10);
    }

    #[test]
    fn test_scanner_summary_missing_counts_is_rejected() {
        assert!(serde_json::from_str::<ScannerSummary>("{}").is_err());
    }

    // -------------------------------------------------------------------------
    // ScanTotals
    // -------------------------------------------------------------------------

    #[test]
    fn test_scan_totals_total() {
        let totals = ScanTotals {
            critical: 5,
            warning: 100,
            info: 50,
            files: 20,
        };
        assert_eq!(totals.total(), Some(155));
    }

    #[test]
    fn test_scan_totals_has_critical() {
        let with = ScanTotals {
            critical: 1,
            warning: 0,
            info: 0,
            files: 1,
        };
        assert!(with.has_critical());

        let without = ScanTotals {
            critical: 0,
            warning: 100,
            info: 50,
            files: 10,
        };
        assert!(!without.has_critical());
    }

    #[test]
    fn test_scan_totals_default() {
        let totals = ScanTotals::default();
        assert_eq!(totals.total(), Some(0));
        assert!(!totals.has_critical());
    }

    #[test]
    fn test_scan_totals_serde_roundtrip() {
        let totals = ScanTotals {
            critical: 3,
            warning: 50,
            info: 20,
            files: 10,
        };
        let json = serde_json::to_string(&totals).unwrap();
        let back: ScanTotals = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total(), Some(73));
    }

    // -------------------------------------------------------------------------
    // ScanReport
    // -------------------------------------------------------------------------

    #[test]
    fn test_scan_report_deserialize_real_ubs_output() {
        let json = r#"{
            "project": "/tmp/test",
            "timestamp": "2026-02-22 01:26:54",
            "scanners": [
                {
                    "project": "/tmp/test",
                    "files": 249,
                    "critical": 420,
                    "warning": 51683,
                    "info": 12542,
                    "timestamp": "2026-02-22T06:26:54Z",
                    "format": "json",
                    "language": "rust"
                }
            ],
            "totals": {
                "critical": 420,
                "warning": 51683,
                "info": 12542,
                "files": 249
            }
        }"#;
        let report: ScanReport = serde_json::from_str(json).unwrap();
        assert_eq!(report.project, Some("/tmp/test".to_string()));
        assert_eq!(report.scanners.len(), 1);
        assert_eq!(report.totals.critical, 420);
        assert_eq!(report.totals.files, 249);
    }

    #[test]
    fn test_scan_report_missing_scanners_is_rejected() {
        let json = r#"{"totals":{"critical":0,"warning":0,"info":0,"files":0}}"#;
        assert!(serde_json::from_str::<ScanReport>(json).is_err());
        assert!(serde_json::from_str::<ScanReport>("{}").is_err());
    }

    #[test]
    fn test_scan_report_forward_compat() {
        let json = r#"{
            "project": "/x",
            "scanners": [],
            "totals": {"critical":0,"warning":0,"info":0,"files":0},
            "new_top_level_field": true
        }"#;
        let report: ScanReport = serde_json::from_str(json).unwrap();
        assert!(report.extra.contains_key("new_top_level_field"));
    }

    // -------------------------------------------------------------------------
    // ScanClassification
    // -------------------------------------------------------------------------

    #[test]
    fn test_classify_clean() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals::default(),
            extra: HashMap::new(),
        };
        assert_eq!(CodeScanner::classify(&report), ScanClassification::Clean);
    }

    #[test]
    fn test_classify_warning() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals {
                critical: 0,
                warning: 50,
                info: 10,
                files: 5,
            },
            extra: HashMap::new(),
        };
        assert_eq!(CodeScanner::classify(&report), ScanClassification::Warning);
    }

    #[test]
    fn test_classify_high_warning() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals {
                critical: 0,
                warning: 200,
                info: 0,
                files: 10,
            },
            extra: HashMap::new(),
        };
        assert_eq!(
            CodeScanner::classify(&report),
            ScanClassification::HighWarning
        );
    }

    #[test]
    fn test_classify_critical() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals {
                critical: 1,
                warning: 0,
                info: 0,
                files: 1,
            },
            extra: HashMap::new(),
        };
        assert_eq!(CodeScanner::classify(&report), ScanClassification::Critical);
    }

    #[test]
    fn test_classify_critical_overrides_warning() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals {
                critical: 5,
                warning: 500,
                info: 100,
                files: 50,
            },
            extra: HashMap::new(),
        };
        assert_eq!(CodeScanner::classify(&report), ScanClassification::Critical);
    }

    // -------------------------------------------------------------------------
    // CodeScanner construction
    // -------------------------------------------------------------------------

    #[test]
    fn test_code_scanner_new() {
        let scanner = CodeScanner::new();
        assert_eq!(scanner.bridge.binary_name(), "ubs");
    }

    #[test]
    fn test_code_scanner_default() {
        let scanner = CodeScanner::default();
        assert_eq!(scanner.bridge.binary_name(), "ubs");
    }

    // -------------------------------------------------------------------------
    // Availability is explicit; execution failures are tested below.
    // -------------------------------------------------------------------------

    #[test]
    fn test_is_available_false_for_missing() {
        let scanner = CodeScanner::new()
            .with_binary(Path::new("definitely-missing-ubs-binary-xyz"))
            .unwrap();
        assert!(!scanner.is_available());
    }

    // -------------------------------------------------------------------------
    // ScanClassification equality
    // -------------------------------------------------------------------------

    #[test]
    fn test_scan_classification_eq() {
        assert_eq!(ScanClassification::Clean, ScanClassification::Clean);
        assert_eq!(ScanClassification::Critical, ScanClassification::Critical);
        assert_ne!(ScanClassification::Clean, ScanClassification::Critical);
    }

    #[test]
    fn test_scan_classification_copy() {
        let c = ScanClassification::Warning;
        let copy = c;
        assert_eq!(c, copy);
    }

    #[test]
    fn test_scan_classification_debug() {
        let dbg = format!("{:?}", ScanClassification::HighWarning);
        assert!(dbg.contains("HighWarning"));
    }
}
