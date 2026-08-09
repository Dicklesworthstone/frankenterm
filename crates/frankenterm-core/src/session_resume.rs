//! Session resume orchestrator — bridges FrankenTerm ↔ `casr` CLI.
//!
//! Wraps `cross_agent_session_resumer` subprocess calls for discovering,
//! resuming, and exporting agent sessions across providers (Claude Code,
//! Codex, Gemini, etc.).
//!
//! Feature-gated behind `session-resume`.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};

use crate::casr_types::{
    CanonicalMessage, CanonicalSession, CasrListEntry, CasrProviderStatus, CasrResumeOutput,
};
use crate::runtime_async::process::{
    Command, CommandCancellation, CommandCancelled, CommandCleanupTrigger,
    CommandOutputCaptureIncomplete, CommandOutputLimitExceeded, CommandOutputStream,
    CommandProcessCleanupIncomplete, CommandTimedOut, DEFAULT_COMMAND_STDERR_LIMIT_BYTES,
    DEFAULT_COMMAND_STDOUT_LIMIT_BYTES, decode_captured_bytes_lossy,
};

// =============================================================================
// Agent provider enum
// =============================================================================

/// Known AI agent providers supported by casr.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProvider {
    ClaudeCode,
    Codex,
    Gemini,
    #[serde(rename = "agy", alias = "antigravity", alias = "antigravity-cli")]
    Antigravity,
    Grok,
    /// Provider not in the known set.
    Other(String),
}

/// Required Antigravity model pin for native resume commands.
pub const ANTIGRAVITY_MODEL: &str = "Gemini 3.1 Pro (High)";

/// Antigravity CLI binary name used for native resume.
pub const ANTIGRAVITY_BINARY: &str = "agy";

/// Discovery-source tag for native Antigravity conversation DB scans.
pub const ANTIGRAVITY_DISCOVERY_SOURCE: &str = "antigravity_conversations_db";

/// Metadata fallback reason for native Antigravity DB entries.
pub const ANTIGRAVITY_METADATA_FALLBACK_REASON: &str =
    "antigravity_sqlite_schema_unstable_title_not_read";

/// Native Antigravity conversation database location relative to a home dir.
pub const ANTIGRAVITY_CONVERSATIONS_RELATIVE_DIR: [&str; 3] =
    [".gemini", "antigravity-cli", "conversations"];

/// Native provider resume plan with the exact argv surfaced for operator and robot contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeResumePlan {
    pub provider_slug: String,
    pub session_id: String,
    pub binary: String,
    pub argv: Vec<String>,
    pub model_name: Option<String>,
}

impl NativeResumePlan {
    /// Fail closed if the native provider binary is not present on PATH.
    pub fn require_binary_available_in_path(
        &self,
        path_env: Option<&str>,
    ) -> Result<(), SessionResumeError> {
        if binary_exists_on_path(&self.binary, path_env) {
            return Ok(());
        }

        Err(SessionResumeError::NativeProviderNotFound)
    }
}

impl AgentProvider {
    /// The casr CLI slug for this provider.
    pub fn slug(&self) -> &str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Antigravity => "agy",
            Self::Grok => "grok",
            Self::Other(s) => s,
        }
    }

    /// Parse a slug string into an [`AgentProvider`].
    pub fn from_slug(slug: &str) -> Self {
        let trimmed = slug.trim();
        let normalized = trimmed.to_ascii_lowercase();
        match normalized.as_str() {
            "claude-code" | "cc" => Self::ClaudeCode,
            "codex" | "cod" => Self::Codex,
            "gemini" | "gmi" => Self::Gemini,
            "agy" | "antigravity" | "antigravity-cli" => Self::Antigravity,
            "grok" => Self::Grok,
            _ => Self::Other(trimmed.to_string()),
        }
    }

    /// Native provider resume command for providers that do not go through casr.
    pub fn native_resume_command(&self, session_id: &str) -> Option<Vec<String>> {
        self.checked_native_resume_plan(session_id)
            .ok()
            .flatten()
            .map(|plan| plan.argv)
    }

    /// Checked native provider resume plan for providers that do not go through casr.
    pub fn checked_native_resume_plan(
        &self,
        session_id: &str,
    ) -> Result<Option<NativeResumePlan>, SessionResumeError> {
        match self {
            Self::Antigravity => Ok(Some(antigravity_native_resume_plan(session_id)?)),
            _ => Ok(None),
        }
    }
}

impl std::fmt::Display for AgentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.slug())
    }
}

// =============================================================================
// Session resume config
// =============================================================================

/// Configuration for the session resume bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResumeConfig {
    /// Path to the `casr` binary. Defaults to `"casr"` (found via PATH).
    #[serde(default = "default_casr_binary")]
    pub casr_binary: String,
    /// Working directory for subprocess calls (defaults to cwd).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    /// Explicit home directory exported to CASR as `HOME` (and
    /// `USERPROFILE` on Windows). This keeps CLI `--home` selection aligned
    /// across native filesystem discovery and CASR subprocess discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_dir: Option<PathBuf>,
    /// Timeout in seconds for subprocess calls.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Whether to use dry-run mode by default.
    #[serde(default)]
    pub dry_run: bool,
}

fn default_casr_binary() -> String {
    "casr".to_string()
}

fn default_timeout_secs() -> u64 {
    30
}

const CASR_STDOUT_LIMIT_BYTES: usize = DEFAULT_COMMAND_STDOUT_LIMIT_BYTES;
const CASR_STDERR_LIMIT_BYTES: usize = DEFAULT_COMMAND_STDERR_LIMIT_BYTES;

/// Hard admission ceiling for a caller-requested CASR stdout capture budget.
/// Parsing a JSON payload has non-trivial memory amplification, so an embedding
/// caller may widen the default but may not turn this bridge into an effectively
/// unbounded collector.
pub const MAX_CASR_STDOUT_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// Hard admission ceiling for caller-requested CASR stderr capture.
pub const MAX_CASR_STDERR_LIMIT_BYTES: usize = 4 * 1024 * 1024;
/// Hard wall-clock ceiling for one session-resume subprocess. Zero is rejected
/// because it cannot admit useful work; values above this ceiling undermine
/// the interactive cancellation contract.
pub const MAX_SESSION_RESUME_TIMEOUT_SECS: u64 = 30 * 60;
/// Maximum number of filesystem entries examined per native scan and sessions
/// admitted from all discovery sources combined.
pub const MAX_SESSION_DISCOVERY_ENTRIES: usize = 10_000;
/// Maximum number of native Antigravity scans that may own blocking work at
/// once. Admission is fail-fast rather than queued, so caller-future drop can
/// never accumulate an unbounded backlog behind the blocking pool.
pub const MAX_CONCURRENT_NATIVE_DISCOVERY_SCANS: usize = 4;
/// Maximum byte length of one caller-selected native-discovery root.
pub const MAX_NATIVE_DISCOVERY_HOME_PATH_BYTES: usize = 32 * 1024;
/// Maximum number of path components walked while opening a discovery root.
pub const MAX_NATIVE_DISCOVERY_HOME_COMPONENTS: usize = 256;
/// Aggregate directory-entry name bytes charged by one native scan.
pub const MAX_NATIVE_DISCOVERY_ENTRY_NAME_BYTES: usize = 4 * 1024 * 1024;
/// Aggregate bytes charged for result paths retained by one native scan.
pub const MAX_NATIVE_DISCOVERY_RESULT_PATH_BYTES: usize = 16 * 1024 * 1024;
/// Conservative logical metadata charge for one candidate. This accounts for
/// file-type, metadata, and fixed SQLite-header state without depending on a
/// platform-specific `Metadata` representation.
const NATIVE_DISCOVERY_METADATA_CHARGE_PER_ENTRY: usize = 2 * 1024;
/// Aggregate logical metadata bytes charged by one native scan.
pub const MAX_NATIVE_DISCOVERY_METADATA_BYTES: usize =
    MAX_SESSION_DISCOVERY_ENTRIES * NATIVE_DISCOVERY_METADATA_CHARGE_PER_ENTRY;
/// Maximum filesystem-operation exposure charged by one native scan.
pub const MAX_NATIVE_DISCOVERY_SYSCALLS: usize = 64 * 1024;
/// Maximum bytes admitted for a session identifier crossing into argv or a
/// public discovery result.
pub const MAX_SESSION_RESUME_ID_BYTES: usize = 256;
/// Maximum bytes admitted for a provider slug crossing into argv.
pub const MAX_SESSION_RESUME_PROVIDER_BYTES: usize = 64;
const SESSION_RESUME_CX_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SESSION_COMMAND_RUNNING: u8 = 0;
const SESSION_COMMAND_CANCEL_REQUESTED: u8 = 1;
const SESSION_COMMAND_WORKER_SETTLED: u8 = 2;

static NATIVE_DISCOVERY_ACTIVE_SCANS: AtomicUsize = AtomicUsize::new(0);
static NATIVE_DISCOVERY_ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
static NATIVE_DISCOVERY_ACTIVE_OBSERVERS: AtomicUsize = AtomicUsize::new(0);
static NATIVE_DISCOVERY_MAX_ACTIVE_SCANS: AtomicUsize = AtomicUsize::new(0);
static NATIVE_DISCOVERY_ADMITTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static NATIVE_DISCOVERY_COMPLETED_TOTAL: AtomicU64 = AtomicU64::new(0);
static NATIVE_DISCOVERY_CANCEL_REQUESTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static NATIVE_DISCOVERY_DROPPED_OBSERVER_TOTAL: AtomicU64 = AtomicU64::new(0);
static NATIVE_DISCOVERY_UNDELIVERED_RECEIPT_TOTAL: AtomicU64 = AtomicU64::new(0);
static NATIVE_DISCOVERY_SATURATED_TOTAL: AtomicU64 = AtomicU64::new(0);
static NATIVE_DISCOVERY_RUNTIME_REJECTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static NATIVE_DISCOVERY_WORKER_FAILED_TOTAL: AtomicU64 = AtomicU64::new(0);

fn saturating_increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(1))
    });
}

/// Process-local ownership and settlement telemetry for native discovery.
///
/// Every field is content-free: no home path, session identifier, filename,
/// or error payload can cross this observability boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeDiscoveryRuntimeMetrics {
    /// Subsystem-admitted scans whose blocking-work object has not yet settled.
    /// This includes work queued behind the blocking pool as well as work that
    /// has begun running.
    pub active_scans: usize,
    /// Blocking closures that have actually begun executing.
    pub active_workers: usize,
    /// Caller futures still waiting for their typed terminal receipt.
    pub active_observers: usize,
    /// Highest simultaneous admitted scan count observed by this process.
    pub max_active_scans: usize,
    /// Total scans admitted through the subsystem concurrency ceiling. Runtime
    /// admission failures are counted separately in `runtime_rejected_total`.
    pub admitted_total: u64,
    /// Total owner tasks that reached one terminal receipt.
    pub completed_total: u64,
    /// Total private cooperative-cancellation requests.
    pub cancel_requested_total: u64,
    /// Caller futures dropped before observing a terminal receipt.
    pub dropped_observer_total: u64,
    /// Terminal receipts whose caller receiver no longer existed.
    pub undelivered_receipt_total: u64,
    /// Fail-fast rejections at the subsystem concurrency ceiling.
    pub saturated_total: u64,
    /// Admissions rejected because no live runtime region accepted the owner.
    pub runtime_rejected_total: u64,
    /// Blocking joins that failed before returning a scanner result.
    pub worker_failed_total: u64,
}

/// Snapshot native-discovery ownership telemetry without acquiring a lock.
#[must_use]
pub fn native_discovery_runtime_metrics() -> NativeDiscoveryRuntimeMetrics {
    NativeDiscoveryRuntimeMetrics {
        active_scans: NATIVE_DISCOVERY_ACTIVE_SCANS.load(Ordering::Acquire),
        active_workers: NATIVE_DISCOVERY_ACTIVE_WORKERS.load(Ordering::Acquire),
        active_observers: NATIVE_DISCOVERY_ACTIVE_OBSERVERS.load(Ordering::Acquire),
        max_active_scans: NATIVE_DISCOVERY_MAX_ACTIVE_SCANS.load(Ordering::Acquire),
        admitted_total: NATIVE_DISCOVERY_ADMITTED_TOTAL.load(Ordering::Relaxed),
        completed_total: NATIVE_DISCOVERY_COMPLETED_TOTAL.load(Ordering::Relaxed),
        cancel_requested_total: NATIVE_DISCOVERY_CANCEL_REQUESTED_TOTAL.load(Ordering::Relaxed),
        dropped_observer_total: NATIVE_DISCOVERY_DROPPED_OBSERVER_TOTAL.load(Ordering::Relaxed),
        undelivered_receipt_total: NATIVE_DISCOVERY_UNDELIVERED_RECEIPT_TOTAL
            .load(Ordering::Relaxed),
        saturated_total: NATIVE_DISCOVERY_SATURATED_TOTAL.load(Ordering::Relaxed),
        runtime_rejected_total: NATIVE_DISCOVERY_RUNTIME_REJECTED_TOTAL.load(Ordering::Relaxed),
        worker_failed_total: NATIVE_DISCOVERY_WORKER_FAILED_TOTAL.load(Ordering::Relaxed),
    }
}

/// Finite identity for a session-discovery source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDiscoverySource {
    Casr,
    NativeAntigravity,
    Merged,
}

/// Finite resource class for native discovery admission failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDiscoveryResource {
    HomePathBytes,
    HomePathComponents,
    EntryNameBytes,
    ResultPathBytes,
    MetadataBytes,
    Syscalls,
}

/// Finite reason a native scan could not acquire structured runtime ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDiscoveryAdmissionRejection {
    SubsystemSaturated,
    RuntimeUnavailableOrShuttingDown,
    RuntimeAtCapacity,
    RuntimeRejected,
}

impl std::fmt::Display for SessionDiscoveryAdmissionRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SubsystemSaturated => "subsystem_saturated",
            Self::RuntimeUnavailableOrShuttingDown => "runtime_unavailable_or_shutting_down",
            Self::RuntimeAtCapacity => "runtime_at_capacity",
            Self::RuntimeRejected => "runtime_rejected",
        })
    }
}

impl std::fmt::Display for SessionDiscoveryResource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::HomePathBytes => "home_path_bytes",
            Self::HomePathComponents => "home_path_components",
            Self::EntryNameBytes => "entry_name_bytes",
            Self::ResultPathBytes => "result_path_bytes",
            Self::MetadataBytes => "metadata_bytes",
            Self::Syscalls => "syscalls",
        })
    }
}

impl std::fmt::Display for SessionDiscoverySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Casr => "casr",
            Self::NativeAntigravity => "native_antigravity",
            Self::Merged => "merged",
        })
    }
}

/// Why one discovery source could not provide a complete result.
///
/// These labels deliberately carry no executable path, home directory,
/// session identifier, or subprocess output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDiscoveryIncompleteReason {
    Unavailable,
    TimedOut,
    SubprocessFailed,
    Cancelled,
    AsyncInfrastructureFailure,
    CleanupIncomplete,
    InvalidConfiguration,
    InvalidOutput,
    OutputCaptureIncomplete,
    LimitExceeded,
    RequestedTargetAbsent,
    DirectoryUnreadable,
    DirectoryEntryUnreadable,
    SymlinkRejected,
}

impl std::fmt::Display for SessionDiscoveryIncompleteReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "unavailable",
            Self::TimedOut => "timed_out",
            Self::SubprocessFailed => "subprocess_failed",
            Self::Cancelled => "cancelled",
            Self::AsyncInfrastructureFailure => "async_infrastructure_failure",
            Self::CleanupIncomplete => "cleanup_incomplete",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::InvalidOutput => "invalid_output",
            Self::OutputCaptureIncomplete => "output_capture_incomplete",
            Self::LimitExceeded => "limit_exceeded",
            Self::RequestedTargetAbsent => "requested_target_absent",
            Self::DirectoryUnreadable => "directory_unreadable",
            Self::DirectoryEntryUnreadable => "directory_entry_unreadable",
            Self::SymlinkRejected => "symlink_rejected",
        })
    }
}

/// Explicit evidence that a returned discovery report is partial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionDiscoveryIncomplete {
    pub source: SessionDiscoverySource,
    pub reason: SessionDiscoveryIncompleteReason,
}

/// Bounded discovery result. Callers must consult [`Self::is_complete`] before
/// presenting the entries as an exhaustive inventory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionDiscoveryResult {
    pub entries: Vec<CasrListEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<SessionDiscoveryIncomplete>,
}

impl SessionDiscoveryResult {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.incomplete.is_empty()
    }

    /// Construct a non-authoritative empty report from a discovery failure.
    /// This is the explicit fail-open conversion used by unattended callers.
    #[must_use]
    pub fn fail_open_from_error(error: &SessionResumeError) -> Self {
        let (source, reason) = discovery_error_incomplete_evidence(error);
        let mut report = Self::default();
        report.mark_incomplete(source, reason);
        report
    }

    /// Prove that an absent entry came from an exhaustive inventory. List
    /// callers may retain partial entries; mutation callers should invoke this
    /// only after their requested target was not found.
    pub fn require_complete_for_absence_claim(&self) -> Result<(), SessionResumeError> {
        match self.incomplete.first() {
            Some(evidence) => Err(SessionResumeError::DiscoveryIncomplete {
                source: evidence.source,
                reason: evidence.reason,
            }),
            None => Ok(()),
        }
    }

    /// Project a merged discovery report onto one provider while preserving
    /// only incompleteness evidence that can affect absence authority for that
    /// provider. Native Antigravity scan failures cannot invalidate a CASR-only
    /// provider inventory, and CASR failures cannot invalidate the native
    /// Antigravity inventory. Merged-limit evidence remains relevant to every
    /// projection.
    pub fn retain_provider(&mut self, provider: &AgentProvider) {
        self.entries
            .retain(|entry| &provider_from_list_entry(entry) == provider);
        self.incomplete.retain(|evidence| {
            evidence.source == SessionDiscoverySource::Merged
                || match provider {
                    AgentProvider::Antigravity => {
                        evidence.source == SessionDiscoverySource::NativeAntigravity
                    }
                    _ => evidence.source == SessionDiscoverySource::Casr,
                }
        });
    }

    fn mark_incomplete(
        &mut self,
        source: SessionDiscoverySource,
        reason: SessionDiscoveryIncompleteReason,
    ) {
        let evidence = SessionDiscoveryIncomplete { source, reason };
        if !self.incomplete.contains(&evidence) {
            self.incomplete.push(evidence);
        }
    }
}

/// Finite reason that a native Antigravity conversation identifier was
/// rejected before it could cross into argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSessionIdInvalidReason {
    WrongShape,
}

impl std::fmt::Display for NativeSessionIdInvalidReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::WrongShape => "wrong_shape",
        })
    }
}

/// Build the checked native Antigravity resume plan with the mandatory model pin.
pub fn antigravity_native_resume_plan(
    session_id: &str,
) -> Result<NativeResumePlan, SessionResumeError> {
    antigravity_native_resume_plan_with_model(session_id, ANTIGRAVITY_MODEL)
}

/// Build the native Antigravity resume plan, rejecting all non-pinned model overrides.
pub fn antigravity_native_resume_plan_with_model(
    session_id: &str,
    model_name: &str,
) -> Result<NativeResumePlan, SessionResumeError> {
    if session_id.len() != 36
        || session_id.trim() != session_id
        || !is_valid_antigravity_conversation_id(session_id)
    {
        return Err(SessionResumeError::InvalidNativeSessionId {
            input_bytes: session_id.len(),
            reason: NativeSessionIdInvalidReason::WrongShape,
        });
    }

    if model_name != ANTIGRAVITY_MODEL {
        return Err(SessionResumeError::NonPinnedNativeModel {
            requested_model_bytes: model_name.len(),
        });
    }

    Ok(NativeResumePlan {
        provider_slug: AgentProvider::Antigravity.slug().to_string(),
        session_id: session_id.to_string(),
        binary: ANTIGRAVITY_BINARY.to_string(),
        argv: vec![
            ANTIGRAVITY_BINARY.to_string(),
            "--conversation".to_string(),
            session_id.to_string(),
            "--model".to_string(),
            ANTIGRAVITY_MODEL.to_string(),
        ],
        model_name: Some(ANTIGRAVITY_MODEL.to_string()),
    })
}

/// Return true for canonical UUID filename stems used by Antigravity conversations.
pub fn is_valid_antigravity_conversation_id(session_id: &str) -> bool {
    let bytes = session_id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }

    for (idx, byte) in bytes.iter().enumerate() {
        match idx {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }

    true
}

fn binary_exists_on_path(binary: &str, path_env: Option<&str>) -> bool {
    let binary_path = Path::new(binary);
    if binary_path.components().count() > 1 {
        return is_executable_binary(binary_path);
    }

    let Some(paths) = path_env
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
    else {
        return false;
    };

    std::env::split_paths(&paths).any(|dir| is_executable_binary(&dir.join(binary)))
}

#[cfg(unix)]
fn is_executable_binary(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let metadata = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) | Err(_) => return false,
    };
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable_binary(path: &Path) -> bool {
    path.is_file()
}

impl Default for SessionResumeConfig {
    fn default() -> Self {
        Self {
            casr_binary: default_casr_binary(),
            working_dir: None,
            home_dir: None,
            timeout_secs: default_timeout_secs(),
            dry_run: false,
        }
    }
}

// =============================================================================
// Recorder CASR export
// =============================================================================

/// Recorder data exported in CASR-compatible format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderCasrExport {
    /// Session metadata.
    pub session: CanonicalSession,
    /// Export generation timestamp (epoch ms).
    pub exported_at: i64,
    /// Source recorder pane IDs included.
    pub pane_ids: Vec<u64>,
    /// Total events processed.
    pub events_processed: usize,
    /// Warnings generated during export.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

// =============================================================================
// Session resume orchestrator
// =============================================================================

/// Orchestrates session discovery, resume, and export via the `casr` CLI.
#[derive(Debug, Clone)]
pub struct SessionResumer {
    config: SessionResumeConfig,
    stdout_limit: usize,
    stderr_limit: usize,
}

/// Finite, content-free reason that a mutating CASR resume may have taken
/// effect even though FrankenTerm could not retain authoritative completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeEffectIndeterminateCause {
    NonZeroExit,
    TimedOut,
    Cancelled,
    CaptureLimitExceeded,
    CaptureIncomplete,
    CleanupIncomplete,
    AsyncInfrastructureFailure,
    InvalidOutput,
}

impl std::fmt::Display for ResumeEffectIndeterminateCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NonZeroExit => "non_zero_exit",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::CaptureLimitExceeded => "capture_limit_exceeded",
            Self::CaptureIncomplete => "capture_incomplete",
            Self::CleanupIncomplete => "cleanup_incomplete",
            Self::AsyncInfrastructureFailure => "async_infrastructure_failure",
            Self::InvalidOutput => "invalid_output",
        })
    }
}

/// Error type for session resume operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionResumeError {
    /// The casr binary was not found or not executable.
    CasrNotFound,
    /// The subprocess exited with a non-zero code.
    SubprocessFailed { code: Option<i32> },
    /// Failed to parse JSON output from casr.
    ParseError { output_bytes: usize },
    /// CASR returned a syntactically valid resume envelope that explicitly
    /// reports that the requested operation did not succeed.
    ResumeRejected,
    /// A non-dry-run resume crossed its external-mutation boundary but no
    /// authoritative completion result was retained. Retrying may duplicate
    /// or conflict with an already-created provider session.
    ResumeEffectIndeterminate {
        cause: ResumeEffectIndeterminateCause,
    },
    /// The requested session was not found.
    SessionNotFound { identifier_bytes: usize },
    /// Provider is not installed.
    ProviderNotInstalled,
    /// Native provider binary was not found.
    NativeProviderNotFound,
    /// Native resume requires an owned interactive terminal/PTY; piping the
    /// provider through captured stdout/stderr would not constitute a usable
    /// resumed session.
    NativeInteractiveTerminalRequired,
    /// Native provider session id is malformed or unsafe.
    InvalidNativeSessionId {
        input_bytes: usize,
        reason: NativeSessionIdInvalidReason,
    },
    /// Native provider model override violated the provider contract.
    NonPinnedNativeModel { requested_model_bytes: usize },
    /// A general CASR session identifier was rejected before argv assembly.
    InvalidSessionIdentifier { input_bytes: usize },
    /// A provider slug was rejected before argv assembly.
    InvalidProviderSlug { input_bytes: usize },
    /// A configured working directory is unavailable or is not a directory.
    WorkingDirectoryUnavailable,
    /// A configured or explicitly selected home is empty or relative, so it
    /// cannot name one filesystem and CASR environment authority independent
    /// of the subprocess working directory.
    InvalidHomeDirectory,
    /// A caller-requested capture limit exceeded the bridge's hard admission
    /// ceiling.
    InvalidOutputLimit {
        stream: CommandOutputStream,
        requested: usize,
        maximum: usize,
    },
    /// A configured subprocess deadline was zero or exceeded the hard
    /// interactive admission ceiling.
    InvalidTimeout { requested: u64, maximum: u64 },
    /// The subprocess crossed an admitted capture limit.
    CaptureLimitExceeded {
        stream: CommandOutputStream,
        observed: usize,
        limit: usize,
    },
    /// A discovery source or merged result crossed the bounded entry ceiling.
    DiscoveryLimitExceeded {
        source: SessionDiscoverySource,
        limit: usize,
    },
    /// A native filesystem scan crossed a finite non-entry resource budget.
    DiscoveryResourceLimitExceeded {
        resource: SessionDiscoveryResource,
        observed: usize,
        limit: usize,
    },
    /// No live structured owner accepted the native scan before filesystem
    /// effects began.
    DiscoveryAdmissionRejected {
        reason: SessionDiscoveryAdmissionRejection,
    },
    /// A partial inventory cannot prove that an absent mutation target does
    /// not exist.
    DiscoveryIncomplete {
        source: SessionDiscoverySource,
        reason: SessionDiscoveryIncompleteReason,
    },
    /// The async blocking executor or cancellation bridge failed without a
    /// trustworthy caller-cancellation classification.
    AsyncInfrastructureFailure,
    /// Operation exceeded its configured wall-clock deadline.
    Timeout,
    /// Operation was deliberately cancelled by its caller.
    Cancelled,
    /// The child leader exited but inherited output descriptors did not close
    /// inside the finite post-exit drain window.
    CaptureIncomplete {
        stdout_open: bool,
        stderr_open: bool,
        drain_timeout_ms: u64,
    },
    /// The initiating failure was observed, but bounded process cleanup could
    /// not prove leader reap, process-tree signalling, and capture closure.
    CleanupIncomplete {
        trigger: CommandCleanupTrigger,
        leader_reaped: bool,
        signal_helper_settled: bool,
        process_tree_signalled: bool,
        stdout_open: bool,
        stderr_open: bool,
        settle_timeout_ms: u64,
    },
}

impl std::fmt::Display for SessionResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CasrNotFound => f.write_str("casr command unavailable"),
            Self::SubprocessFailed { code } => {
                write!(f, "casr subprocess failed (exit {})", code.unwrap_or(-1))
            }
            Self::ParseError { output_bytes } => {
                write!(f, "casr returned invalid JSON ({output_bytes} bytes)")
            }
            Self::ResumeRejected => f.write_str("casr reported that resume failed"),
            Self::ResumeEffectIndeterminate { cause } => {
                write!(f, "casr resume effect is indeterminate (cause={cause})")
            }
            Self::SessionNotFound { identifier_bytes } => {
                write!(f, "session not found (identifier_bytes={identifier_bytes})")
            }
            Self::ProviderNotInstalled => f.write_str("provider not installed"),
            Self::NativeProviderNotFound => f.write_str("native provider binary unavailable"),
            Self::NativeInteractiveTerminalRequired => {
                f.write_str("native resume requires an owned interactive terminal")
            }
            Self::InvalidNativeSessionId {
                input_bytes,
                reason,
            } => write!(
                f,
                "invalid native provider session id (input_bytes={input_bytes}, reason={reason})"
            ),
            Self::NonPinnedNativeModel {
                requested_model_bytes,
            } => write!(
                f,
                "native provider model is not the required pinned model (requested_bytes={requested_model_bytes})"
            ),
            Self::InvalidSessionIdentifier { input_bytes } => {
                write!(f, "invalid session identifier (input_bytes={input_bytes})")
            }
            Self::InvalidProviderSlug { input_bytes } => {
                write!(f, "invalid provider slug (input_bytes={input_bytes})")
            }
            Self::WorkingDirectoryUnavailable => {
                f.write_str("session-resume working directory unavailable")
            }
            Self::InvalidHomeDirectory => {
                f.write_str("session-resume home directory must be a non-empty absolute path")
            }
            Self::InvalidOutputLimit {
                stream,
                requested,
                maximum,
            } => write!(
                f,
                "invalid casr {stream} capture limit ({requested} > {maximum})"
            ),
            Self::InvalidTimeout { requested, maximum } => write!(
                f,
                "invalid session-resume timeout (requested_seconds={requested}, maximum_seconds={maximum})"
            ),
            Self::CaptureLimitExceeded {
                stream,
                observed,
                limit,
            } => write!(
                f,
                "casr {stream} capture limit exceeded (observed_at_least={observed}, limit={limit})"
            ),
            Self::DiscoveryLimitExceeded { source, limit } => write!(
                f,
                "{source} session discovery exceeded the {limit}-entry limit"
            ),
            Self::DiscoveryResourceLimitExceeded {
                resource,
                observed,
                limit,
            } => write!(
                f,
                "native session discovery exceeded the {resource} budget (observed={observed}, limit={limit})"
            ),
            Self::DiscoveryAdmissionRejected { reason } => write!(
                f,
                "native session discovery admission rejected ({reason})"
            ),
            Self::DiscoveryIncomplete { source, reason } => write!(
                f,
                "session discovery incomplete (source={source}, reason={reason})"
            ),
            Self::AsyncInfrastructureFailure => {
                f.write_str("session-resume async infrastructure failed")
            }
            Self::Timeout => write!(f, "casr operation timed out"),
            Self::Cancelled => write!(f, "casr operation cancelled"),
            Self::CaptureIncomplete {
                stdout_open,
                stderr_open,
                drain_timeout_ms,
            } => write!(
                f,
                "casr output capture incomplete after {drain_timeout_ms} ms (stdout_open={stdout_open}, stderr_open={stderr_open})"
            ),
            Self::CleanupIncomplete {
                trigger,
                leader_reaped,
                signal_helper_settled,
                process_tree_signalled,
                stdout_open,
                stderr_open,
                settle_timeout_ms,
            } => write!(
                f,
                "casr process cleanup incomplete after {settle_timeout_ms} ms (trigger={trigger}, leader_reaped={leader_reaped}, signal_helper_settled={signal_helper_settled}, process_tree_signalled={process_tree_signalled}, stdout_open={stdout_open}, stderr_open={stderr_open})"
            ),
        }
    }
}

impl std::error::Error for SessionResumeError {}

/// Admit a session identifier before it reaches discovery, path resolution,
/// argv construction, or logging. Rejections retain only the input byte count.
pub fn validate_session_identifier(session_id: &str) -> Result<(), SessionResumeError> {
    if session_id.is_empty()
        || session_id.len() > MAX_SESSION_RESUME_ID_BYTES
        || session_id.trim() != session_id
        || session_id.chars().any(char::is_control)
    {
        return Err(SessionResumeError::InvalidSessionIdentifier {
            input_bytes: session_id.len(),
        });
    }
    Ok(())
}

/// Admit a provider slug before it reaches discovery, path resolution, argv
/// construction, or logging. Rejections retain only the input byte count.
pub fn validate_provider_slug(provider_slug: &str) -> Result<(), SessionResumeError> {
    if provider_slug.is_empty()
        || provider_slug.len() > MAX_SESSION_RESUME_PROVIDER_BYTES
        || !provider_slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(SessionResumeError::InvalidProviderSlug {
            input_bytes: provider_slug.len(),
        });
    }
    Ok(())
}

/// Validate and materialize the finite subprocess deadline shared by CASR and
/// native-provider runners.
pub fn validate_session_resume_timeout_secs(
    timeout_secs: u64,
) -> Result<Duration, SessionResumeError> {
    if timeout_secs == 0 || timeout_secs > MAX_SESSION_RESUME_TIMEOUT_SECS {
        return Err(SessionResumeError::InvalidTimeout {
            requested: timeout_secs,
            maximum: MAX_SESSION_RESUME_TIMEOUT_SECS,
        });
    }
    Ok(Duration::from_secs(timeout_secs))
}

fn validate_session_resume_home(home_dir: &Path) -> Result<(), SessionResumeError> {
    if home_dir.as_os_str().is_empty() || !home_dir.is_absolute() {
        return Err(SessionResumeError::InvalidHomeDirectory);
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct NativeDiscoveryResourceBudget {
    entry_name_bytes: usize,
    result_path_bytes: usize,
    metadata_bytes: usize,
    syscalls: usize,
}

impl NativeDiscoveryResourceBudget {
    fn for_root(root: &Path) -> Result<Self, SessionResumeError> {
        let path_bytes = root.as_os_str().as_encoded_bytes().len();
        if path_bytes > MAX_NATIVE_DISCOVERY_HOME_PATH_BYTES {
            return Err(SessionResumeError::DiscoveryResourceLimitExceeded {
                resource: SessionDiscoveryResource::HomePathBytes,
                observed: path_bytes,
                limit: MAX_NATIVE_DISCOVERY_HOME_PATH_BYTES,
            });
        }
        let components = root.components().count();
        if components > MAX_NATIVE_DISCOVERY_HOME_COMPONENTS {
            return Err(SessionResumeError::DiscoveryResourceLimitExceeded {
                resource: SessionDiscoveryResource::HomePathComponents,
                observed: components,
                limit: MAX_NATIVE_DISCOVERY_HOME_COMPONENTS,
            });
        }

        let mut budget = Self::default();
        // Opening each component performs at most one open plus one metadata
        // verification. Charge the root/base operation as one component too.
        budget.charge_syscalls(components.saturating_add(1).saturating_mul(2))?;
        Ok(budget)
    }

    fn charge_fixed_child(&mut self, component: &str) -> Result<(), SessionResumeError> {
        self.charge_entry_name_bytes(component.len())?;
        self.charge_syscalls(2)
    }

    fn charge_directory_entry(
        &mut self,
        file_name: &std::ffi::OsStr,
    ) -> Result<(), SessionResumeError> {
        self.charge_entry_name_bytes(file_name.as_encoded_bytes().len())?;
        self.charge_syscalls(1)
    }

    fn charge_candidate_metadata(&mut self) -> Result<(), SessionResumeError> {
        self.metadata_bytes = checked_discovery_resource_add(
            SessionDiscoveryResource::MetadataBytes,
            self.metadata_bytes,
            NATIVE_DISCOVERY_METADATA_CHARGE_PER_ENTRY,
            MAX_NATIVE_DISCOVERY_METADATA_BYTES,
        )?;
        // file_type + no-follow open + metadata + fixed header read.
        self.charge_syscalls(4)
    }

    fn charge_result_path(&mut self, path: &Path) -> Result<(), SessionResumeError> {
        self.result_path_bytes = checked_discovery_resource_add(
            SessionDiscoveryResource::ResultPathBytes,
            self.result_path_bytes,
            path.as_os_str().as_encoded_bytes().len(),
            MAX_NATIVE_DISCOVERY_RESULT_PATH_BYTES,
        )?;
        Ok(())
    }

    fn charge_entry_name_bytes(&mut self, bytes: usize) -> Result<(), SessionResumeError> {
        self.entry_name_bytes = checked_discovery_resource_add(
            SessionDiscoveryResource::EntryNameBytes,
            self.entry_name_bytes,
            bytes,
            MAX_NATIVE_DISCOVERY_ENTRY_NAME_BYTES,
        )?;
        Ok(())
    }

    fn charge_syscalls(&mut self, count: usize) -> Result<(), SessionResumeError> {
        self.syscalls = checked_discovery_resource_add(
            SessionDiscoveryResource::Syscalls,
            self.syscalls,
            count,
            MAX_NATIVE_DISCOVERY_SYSCALLS,
        )?;
        Ok(())
    }
}

fn checked_discovery_resource_add(
    resource: SessionDiscoveryResource,
    current: usize,
    additional: usize,
    limit: usize,
) -> Result<usize, SessionResumeError> {
    let observed = current.checked_add(additional).unwrap_or(usize::MAX);
    if observed > limit {
        return Err(SessionResumeError::DiscoveryResourceLimitExceeded {
            resource,
            observed,
            limit,
        });
    }
    Ok(observed)
}

fn map_spawn_blocking_error(
    error: &crate::runtime_async::SpawnBlockingWithCxError,
) -> SessionResumeError {
    use crate::runtime_async::SpawnBlockingWithCxError;

    match error {
        SpawnBlockingWithCxError::CancelledBeforeSpawn { kind }
        | SpawnBlockingWithCxError::CancelledMidFlight { kind } => {
            session_resume_cx_termination_from_kind(*kind)
        }
        SpawnBlockingWithCxError::RuntimeFailure
        | SpawnBlockingWithCxError::CancellationWatcherTimerFailure => {
            SessionResumeError::AsyncInfrastructureFailure
        }
    }
}

fn session_resume_cx_termination_from_kind(
    kind: Option<crate::outcome::CancelKind>,
) -> SessionResumeError {
    use crate::outcome::CancelKind;

    match kind {
        Some(CancelKind::Deadline | CancelKind::Timeout) => SessionResumeError::Timeout,
        Some(CancelKind::PollQuota | CancelKind::CostBudget) => {
            SessionResumeError::AsyncInfrastructureFailure
        }
        Some(
            CancelKind::User
            | CancelKind::FailFast
            | CancelKind::RaceLost
            | CancelKind::ParentCancelled
            | CancelKind::ResourceUnavailable
            | CancelKind::Shutdown
            | CancelKind::LinkedExit,
        )
        | None => SessionResumeError::Cancelled,
    }
}

fn session_resume_cx_termination(cx: &crate::cx::Cx) -> SessionResumeError {
    session_resume_cx_termination_from_kind(cx.root_cancel_cause().map(|reason| reason.kind))
}

struct SessionCommandCancellationGuard {
    cancellation: CommandCancellation,
    watcher_done: Arc<AtomicBool>,
    armed: bool,
}

impl SessionCommandCancellationGuard {
    fn new(cancellation: CommandCancellation, watcher_done: Arc<AtomicBool>) -> Self {
        Self {
            cancellation,
            watcher_done,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionCommandCancellationGuard {
    fn drop(&mut self) {
        self.watcher_done.store(true, Ordering::SeqCst);
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

impl SessionResumer {
    /// Create a new resumer with the given config.
    pub fn new(config: SessionResumeConfig) -> Self {
        Self {
            config,
            stdout_limit: CASR_STDOUT_LIMIT_BYTES,
            stderr_limit: CASR_STDERR_LIMIT_BYTES,
        }
    }

    /// Create a resumer with default config.
    pub fn with_defaults() -> Self {
        Self::new(SessionResumeConfig::default())
    }

    /// Access the config.
    pub fn config(&self) -> &SessionResumeConfig {
        &self.config
    }

    /// Override subprocess capture limits for unusually large CASR datasets or
    /// stricter embedding contexts.
    pub fn with_output_limits(
        mut self,
        stdout_limit: usize,
        stderr_limit: usize,
    ) -> Result<Self, SessionResumeError> {
        if stdout_limit > MAX_CASR_STDOUT_LIMIT_BYTES {
            return Err(SessionResumeError::InvalidOutputLimit {
                stream: CommandOutputStream::Stdout,
                requested: stdout_limit,
                maximum: MAX_CASR_STDOUT_LIMIT_BYTES,
            });
        }
        if stderr_limit > MAX_CASR_STDERR_LIMIT_BYTES {
            return Err(SessionResumeError::InvalidOutputLimit {
                stream: CommandOutputStream::Stderr,
                requested: stderr_limit,
                maximum: MAX_CASR_STDERR_LIMIT_BYTES,
            });
        }
        self.stdout_limit = stdout_limit;
        self.stderr_limit = stderr_limit;
        Ok(self)
    }

    /// Discover sessions across all installed providers.
    ///
    /// Native Antigravity discovery is independent of CASR. A CASR failure
    /// therefore returns a typed partial report instead of discarding valid
    /// native entries.
    pub fn discover_sessions(&self) -> Result<SessionDiscoveryResult, SessionResumeError> {
        match self.effective_discovery_home() {
            Some(home_dir) => self.discover_sessions_in_home(&home_dir),
            None => self.discover_sessions_with_native_antigravity(unavailable_native_discovery()),
        }
    }

    /// Discover sessions using an explicit home directory for native provider scans.
    ///
    /// This is useful for tests and automation that need deterministic provider
    /// fixtures without mutating process-wide `HOME`.
    pub fn discover_sessions_in_home(
        &self,
        home_dir: &Path,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        validate_session_resume_home(home_dir)?;
        let scoped = self.scoped_to_discovery_home(home_dir);
        scoped.discover_sessions_with_native_antigravity(
            discover_antigravity_conversations_from_home(home_dir)?,
        )
    }

    /// Cx-first discovery. Filesystem scanning and JSON parsing stay off the
    /// async worker; CASR execution observes both the configured wall timeout
    /// and caller cancellation through the canonical command supervisor.
    pub async fn discover_sessions_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        match self.effective_discovery_home() {
            Some(home_dir) => self.discover_sessions_in_home_with_cx(cx, &home_dir).await,
            None => {
                self.discover_sessions_with_native_antigravity_with_cx(
                    cx,
                    unavailable_native_discovery(),
                )
                .await
            }
        }
    }

    /// Cx-first discovery rooted at an explicit home directory.
    pub async fn discover_sessions_in_home_with_cx(
        &self,
        cx: &crate::cx::Cx,
        home_dir: &Path,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        validate_session_resume_home(home_dir)?;
        let native = discover_antigravity_conversations_from_home_with_cx(cx, home_dir).await?;
        let scoped = self.scoped_to_discovery_home(home_dir);
        scoped
            .discover_sessions_with_native_antigravity_with_cx(cx, native)
            .await
    }

    /// Resolve the single home authority for a default discovery call. An
    /// explicit config value wins; otherwise the platform process-home
    /// resolver is consulted once and the resulting path is used by both the
    /// native scanner and CASR.
    fn effective_discovery_home(&self) -> Option<PathBuf> {
        self.config
            .home_dir
            .clone()
            .or_else(session_resume_home_dir)
    }

    /// Return an operation-scoped resumer whose CASR environment is bound to
    /// the same home used by the native scan. This deliberately overrides a
    /// different configured home for the explicit-home public APIs; merging
    /// inventories from two homes would make absence claims unsound.
    fn scoped_to_discovery_home(&self, home_dir: &Path) -> Self {
        let mut scoped = self.clone();
        scoped.config.home_dir = Some(home_dir.to_path_buf());
        scoped
    }

    fn discover_sessions_with_native_antigravity(
        &self,
        mut native: SessionDiscoveryResult,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        info!(session_resume = true, "discovering sessions");

        match self.run_casr(&["list", "--json"]) {
            Ok(output) => match parse_casr_discovery_entries(&output) {
                Ok(casr_entries) => {
                    merge_session_discovery_entries(&mut native, casr_entries)?;
                }
                Err(error) => {
                    let Some(reason) = casr_discovery_incomplete_reason(&error) else {
                        return Err(error);
                    };
                    native.mark_incomplete(SessionDiscoverySource::Casr, reason);
                }
            },
            Err(error) => {
                let Some(reason) = casr_discovery_incomplete_reason(&error) else {
                    return Err(error);
                };
                native.mark_incomplete(SessionDiscoverySource::Casr, reason);
            }
        }

        info!(
            session_resume = true,
            sessions_found = native.entries.len(),
            discovery_complete = native.is_complete(),
            "discovered sessions"
        );
        Ok(native)
    }

    async fn discover_sessions_with_native_antigravity_with_cx(
        &self,
        cx: &crate::cx::Cx,
        mut native: SessionDiscoveryResult,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        info!(session_resume = true, "discovering sessions");

        match self.run_casr_with_cx(cx, &["list", "--json"]).await {
            Ok(output) => {
                let parsed = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
                    parse_casr_discovery_entries(&output)
                })
                .await
                .map_err(|error| map_spawn_blocking_error(&error))?;
                match parsed {
                    Ok(casr_entries) => {
                        merge_session_discovery_entries(&mut native, casr_entries)?;
                    }
                    Err(error) => {
                        let Some(reason) = casr_discovery_incomplete_reason(&error) else {
                            return Err(error);
                        };
                        native.mark_incomplete(SessionDiscoverySource::Casr, reason);
                    }
                }
            }
            Err(error) => {
                let Some(reason) = casr_discovery_incomplete_reason(&error) else {
                    return Err(error);
                };
                native.mark_incomplete(SessionDiscoverySource::Casr, reason);
            }
        }

        info!(
            session_resume = true,
            sessions_found = native.entries.len(),
            discovery_complete = native.is_complete(),
            "discovered sessions"
        );
        Ok(native)
    }

    /// Discover sessions filtered by provider.
    pub fn discover_sessions_for_provider(
        &self,
        provider: &AgentProvider,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        let slug = provider.slug();
        validate_provider_slug(slug)?;
        let mut report = self.discover_sessions()?;
        report.retain_provider(provider);
        Ok(report)
    }

    /// Cx-first provider-filtered discovery.
    pub async fn discover_sessions_for_provider_with_cx(
        &self,
        cx: &crate::cx::Cx,
        provider: &AgentProvider,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        let slug = provider.slug();
        validate_provider_slug(slug)?;
        let mut report = self.discover_sessions_with_cx(cx).await?;
        report.retain_provider(provider);
        Ok(report)
    }

    /// Resume a session into a target provider.
    ///
    /// Calls `casr resume <session_id> --target <provider> --json`.
    pub fn resume_session(
        &self,
        session_id: &str,
        target_provider: &AgentProvider,
    ) -> Result<CasrResumeOutput, SessionResumeError> {
        validate_session_identifier(session_id)?;
        validate_provider_slug(target_provider.slug())?;
        info!(
            session_resume = true,
            dry_run = self.config.dry_run,
            "resuming session"
        );

        let mut args = vec![
            "resume",
            session_id,
            "--target",
            target_provider.slug(),
            "--json",
        ];
        if self.config.dry_run {
            args.push("--dry-run");
        }

        let output = self
            .run_casr(&args)
            .map_err(|error| self.classify_resume_failure(error))?;
        parse_casr_resume_output(&output).map_err(|error| self.classify_resume_failure(error))
    }

    /// Cx-first session resume. The subprocess never blocks the async worker,
    /// and caller cancellation reaches the bounded process supervisor.
    pub async fn resume_session_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session_id: &str,
        target_provider: &AgentProvider,
    ) -> Result<CasrResumeOutput, SessionResumeError> {
        validate_session_identifier(session_id)?;
        validate_provider_slug(target_provider.slug())?;
        info!(
            session_resume = true,
            dry_run = self.config.dry_run,
            "resuming session"
        );

        let mut args = vec![
            "resume",
            session_id,
            "--target",
            target_provider.slug(),
            "--json",
        ];
        if self.config.dry_run {
            args.push("--dry-run");
        }

        let output = self
            .run_casr_with_cx(cx, &args)
            .await
            .map_err(|error| self.classify_resume_failure(error))?;
        // Successful subprocess settlement is the mutation linearization
        // point. From here on, caller cancellation must not turn a completed
        // external resume into `Cancelled` and invite an unsafe retry. Parsing
        // is bounded by the admitted stdout limit and its blocking join is
        // retained until it settles.
        crate::runtime_async::spawn_blocking(move || parse_casr_resume_output(&output))
            .await
            .map_err(|_| {
                self.classify_resume_failure(SessionResumeError::AsyncInfrastructureFailure)
            })?
            .map_err(|error| self.classify_resume_failure(error))
    }

    /// List installed providers.
    ///
    /// Calls `casr providers --json`.
    pub fn list_providers(&self) -> Result<Vec<CasrProviderStatus>, SessionResumeError> {
        let output = self.run_casr(&["providers", "--json"])?;
        let output_bytes = output.len();
        let providers: Vec<CasrProviderStatus> = serde_json::from_str(&output)
            .map_err(|_| SessionResumeError::ParseError { output_bytes })?;
        Ok(providers)
    }

    /// Cx-first provider inventory.
    pub async fn list_providers_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<Vec<CasrProviderStatus>, SessionResumeError> {
        let output = self.run_casr_with_cx(cx, &["providers", "--json"]).await?;
        let output_bytes = output.len();
        crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            serde_json::from_str::<Vec<CasrProviderStatus>>(&output)
                .map_err(|_| SessionResumeError::ParseError { output_bytes })
        })
        .await
        .map_err(|error| map_spawn_blocking_error(&error))?
    }

    /// Check if a specific provider is installed.
    pub fn is_provider_installed(
        &self,
        provider: &AgentProvider,
    ) -> Result<bool, SessionResumeError> {
        validate_provider_slug(provider.slug())?;
        let providers = self.list_providers()?;
        let slug = provider.slug();
        Ok(providers.iter().any(|p| p.slug == slug && p.installed))
    }

    /// Cx-first provider availability check.
    pub async fn is_provider_installed_with_cx(
        &self,
        cx: &crate::cx::Cx,
        provider: &AgentProvider,
    ) -> Result<bool, SessionResumeError> {
        validate_provider_slug(provider.slug())?;
        let providers = self.list_providers_with_cx(cx).await?;
        let slug = provider.slug();
        Ok(providers
            .iter()
            .any(|candidate| candidate.slug == slug && candidate.installed))
    }

    /// Check if `casr` is available on PATH.
    pub fn is_casr_available(&self) -> bool {
        self.run_casr_with_options(&["--version"], false).is_ok()
    }

    /// Cx-first CASR availability probe. Cancellation is not collapsed into a
    /// false availability result.
    pub async fn is_casr_available_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<bool, SessionResumeError> {
        match self
            .run_casr_with_options_with_cx(cx, &["--version"], false)
            .await
        {
            Ok(_) => Ok(true),
            Err(SessionResumeError::Cancelled) => Err(SessionResumeError::Cancelled),
            Err(error @ SessionResumeError::CleanupIncomplete { .. }) => Err(error),
            Err(error @ SessionResumeError::InvalidTimeout { .. }) => Err(error),
            Err(SessionResumeError::AsyncInfrastructureFailure) => {
                Err(SessionResumeError::AsyncInfrastructureFailure)
            }
            Err(_) => Ok(false),
        }
    }

    /// Export recorder data as a CASR-compatible session.
    ///
    /// Converts recorder events into the canonical IR format for portability.
    pub fn export_for_recorder(
        &self,
        session_id: &str,
        provider_slug: &str,
        source_path: &Path,
        messages: Vec<CanonicalMessage>,
        pane_ids: Vec<u64>,
    ) -> RecorderCasrExport {
        let now_ms = chrono::Utc::now().timestamp_millis();

        let session = CanonicalSession {
            session_id: session_id.to_string(),
            provider_slug: provider_slug.to_string(),
            workspace: self.config.working_dir.clone(),
            title: None,
            started_at: messages.first().and_then(|m| m.timestamp),
            ended_at: messages.last().and_then(|m| m.timestamp),
            messages,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            source_path: source_path.to_path_buf(),
            model_name: None,
        };

        let events_processed = session.messages.len();

        RecorderCasrExport {
            session,
            exported_at: now_ms,
            pane_ids,
            events_processed,
            warnings: vec![],
        }
    }

    /// Run a casr subprocess and return stdout on success.
    fn run_casr(&self, args: &[&str]) -> Result<String, SessionResumeError> {
        self.run_casr_with_options(args, true)
    }

    async fn run_casr_with_cx(
        &self,
        cx: &crate::cx::Cx,
        args: &[&str],
    ) -> Result<String, SessionResumeError> {
        self.run_casr_with_options_with_cx(cx, args, true).await
    }

    fn run_casr_with_options(
        &self,
        args: &[&str],
        apply_working_dir: bool,
    ) -> Result<String, SessionResumeError> {
        self.run_casr_with_options_and_cancellation(args, apply_working_dir, None)
    }

    fn run_casr_with_options_and_cancellation(
        &self,
        args: &[&str],
        apply_working_dir: bool,
        cancellation: Option<&CommandCancellation>,
    ) -> Result<String, SessionResumeError> {
        let timeout = validate_session_resume_timeout_secs(self.config.timeout_secs)?;
        let mut cmd = self.build_casr_command(args, apply_working_dir)?;
        let output = match cancellation {
            Some(cancellation) => cmd
                .output_blocking_with_cancellation(timeout, cancellation)
                .map_err(|error| Self::map_command_error(&error))?,
            None => cmd
                .output_blocking(timeout)
                .map_err(|error| Self::map_command_error(&error))?,
        };

        if !output.status.success() {
            return Err(SessionResumeError::SubprocessFailed {
                code: output.status.code(),
            });
        }

        Ok(decode_captured_bytes_lossy(output.stdout))
    }

    async fn run_casr_with_options_with_cx(
        &self,
        cx: &crate::cx::Cx,
        args: &[&str],
        apply_working_dir: bool,
    ) -> Result<String, SessionResumeError> {
        cx.checkpoint()
            .map_err(|_| session_resume_cx_termination(cx))?;
        let timeout = validate_session_resume_timeout_secs(self.config.timeout_secs)?;
        let mut cmd = self.build_casr_command(args, apply_working_dir)?;
        let cancellation = CommandCancellation::new();
        let worker_cancellation = cancellation.clone();
        let watcher_cancellation = cancellation.clone();
        let watcher_done = Arc::new(AtomicBool::new(false));
        let watcher_done_inner = Arc::clone(&watcher_done);
        let command_state = Arc::new(AtomicU8::new(SESSION_COMMAND_RUNNING));
        let watcher_command_state = Arc::clone(&command_state);
        let watcher_cx = cx.clone();
        let mut watcher_guard = SessionCommandCancellationGuard::new(
            watcher_cancellation,
            Arc::clone(&watcher_done_inner),
        );
        let watcher_handle =
            crate::runtime_async::task::try_spawn_with_cx(cx, move |_child_cx| async move {
                while !watcher_done_inner.load(Ordering::SeqCst) {
                    if watcher_cx.checkpoint().is_err() {
                        if watcher_command_state
                            .compare_exchange(
                                SESSION_COMMAND_RUNNING,
                                SESSION_COMMAND_CANCEL_REQUESTED,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            )
                            .is_err()
                        {
                            watcher_guard.disarm();
                            return;
                        }
                        return;
                    }
                    if crate::runtime_async::sleep_with_cx(
                        &watcher_cx,
                        SESSION_RESUME_CX_POLL_INTERVAL,
                    )
                    .await
                    .is_err()
                    {
                        if watcher_command_state
                            .compare_exchange(
                                SESSION_COMMAND_RUNNING,
                                SESSION_COMMAND_CANCEL_REQUESTED,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            )
                            .is_err()
                        {
                            watcher_guard.disarm();
                            return;
                        }
                        // `sleep_with_cx` is budget-aware; an elapsed caller
                        // budget is cancellation of this operation, not an
                        // executor-infrastructure failure.
                        let _ = watcher_cx.checkpoint();
                        return;
                    }
                }
                watcher_guard.disarm();
            })
            .map_err(|_| SessionResumeError::AsyncInfrastructureFailure)?;
        let mut cancellation_guard =
            SessionCommandCancellationGuard::new(cancellation, Arc::clone(&watcher_done));

        // The generic Cx-aware blocking helper deliberately returns before an
        // already-running closure settles. Here the watcher owns Cx-to-command
        // cancellation, so retain the ordinary blocking join until supervised
        // process-tree and pipe cleanup has actually completed.
        let worker_command_state = Arc::clone(&command_state);
        let worker_result = crate::runtime_async::spawn_blocking(move || {
            let result = cmd.output_blocking_with_cancellation(timeout, &worker_cancellation);
            // Compete with cancellation at the actual supervised-command
            // settlement point, before handing the result back to the async
            // scheduler. Marking settlement only after `.await` allowed a
            // late watcher poll to relabel a completed mutation as cancelled.
            let _ = worker_command_state.compare_exchange(
                SESSION_COMMAND_RUNNING,
                SESSION_COMMAND_WORKER_SETTLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            result
        })
        .await;
        watcher_done.store(true, Ordering::SeqCst);
        let watcher_result = watcher_handle.await;
        let cancellation_won =
            command_state.load(Ordering::SeqCst) == SESSION_COMMAND_CANCEL_REQUESTED;

        if worker_result.is_err() || watcher_result.is_err() {
            if cancellation_won {
                return Err(session_resume_cx_termination(cx));
            }
            return Err(SessionResumeError::AsyncInfrastructureFailure);
        }
        let output = worker_result
            .map_err(|_| SessionResumeError::AsyncInfrastructureFailure)?
            .map_err(|error| Self::map_command_error(&error))?;
        if cancellation_won {
            return Err(session_resume_cx_termination(cx));
        }
        cancellation_guard.disarm();

        if !output.status.success() {
            return Err(SessionResumeError::SubprocessFailed {
                code: output.status.code(),
            });
        }
        Ok(decode_captured_bytes_lossy(output.stdout))
    }

    fn build_casr_command(
        &self,
        args: &[&str],
        apply_working_dir: bool,
    ) -> Result<Command, SessionResumeError> {
        let mut cmd = Command::new(&self.config.casr_binary);
        cmd.args(args);
        cmd.kill_on_drop(true);
        cmd.stdout_limit(self.stdout_limit);
        cmd.stderr_limit(self.stderr_limit);

        if apply_working_dir && let Some(ref dir) = self.config.working_dir {
            if !dir.is_dir() {
                return Err(SessionResumeError::WorkingDirectoryUnavailable);
            }
            cmd.current_dir(dir);
        }
        if let Some(ref home_dir) = self.config.home_dir {
            cmd.env("HOME", home_dir);
            #[cfg(windows)]
            cmd.env("USERPROFILE", home_dir);
        }
        Ok(cmd)
    }

    fn map_command_error(err: &std::io::Error) -> SessionResumeError {
        if err.kind() == std::io::ErrorKind::NotFound {
            return SessionResumeError::CasrNotFound;
        }
        if CommandTimedOut::from_io_error(err).is_some() {
            return SessionResumeError::Timeout;
        }
        if CommandCancelled::from_io_error(err).is_some() {
            return SessionResumeError::Cancelled;
        }
        if let Some(incomplete) = CommandOutputCaptureIncomplete::from_io_error(err) {
            return SessionResumeError::CaptureIncomplete {
                stdout_open: incomplete.stdout_open(),
                stderr_open: incomplete.stderr_open(),
                drain_timeout_ms: incomplete.drain_timeout_ms(),
            };
        }
        if let Some(incomplete) = CommandProcessCleanupIncomplete::from_io_error(err) {
            return SessionResumeError::CleanupIncomplete {
                trigger: incomplete.trigger(),
                leader_reaped: incomplete.leader_reaped(),
                signal_helper_settled: incomplete.signal_helper_settled(),
                process_tree_signalled: incomplete.process_tree_signalled(),
                stdout_open: incomplete.stdout_open(),
                stderr_open: incomplete.stderr_open(),
                settle_timeout_ms: incomplete.settle_timeout_ms(),
            };
        }
        if let Some(exceeded) = CommandOutputLimitExceeded::from_io_error(err) {
            return SessionResumeError::CaptureLimitExceeded {
                stream: exceeded.stream(),
                observed: exceeded.observed(),
                limit: exceeded.limit(),
            };
        }

        SessionResumeError::SubprocessFailed { code: None }
    }

    fn classify_resume_failure(&self, error: SessionResumeError) -> SessionResumeError {
        if self.config.dry_run {
            return error;
        }
        let cause = match error {
            SessionResumeError::SubprocessFailed { .. } => {
                ResumeEffectIndeterminateCause::NonZeroExit
            }
            SessionResumeError::Timeout => ResumeEffectIndeterminateCause::TimedOut,
            SessionResumeError::Cancelled => ResumeEffectIndeterminateCause::Cancelled,
            SessionResumeError::CaptureLimitExceeded { .. } => {
                ResumeEffectIndeterminateCause::CaptureLimitExceeded
            }
            SessionResumeError::CaptureIncomplete { .. } => {
                ResumeEffectIndeterminateCause::CaptureIncomplete
            }
            SessionResumeError::CleanupIncomplete { .. } => {
                ResumeEffectIndeterminateCause::CleanupIncomplete
            }
            SessionResumeError::AsyncInfrastructureFailure => {
                ResumeEffectIndeterminateCause::AsyncInfrastructureFailure
            }
            SessionResumeError::ParseError { .. } => ResumeEffectIndeterminateCause::InvalidOutput,
            other => return other,
        };
        SessionResumeError::ResumeEffectIndeterminate { cause }
    }
}

/// Fail-open discovery that preserves typed evidence whenever a source could
/// not be enumerated. The returned entries are never silently presented as a
/// complete inventory.
pub fn discover_sessions_failopen(config: &SessionResumeConfig) -> SessionDiscoveryResult {
    let resumer = SessionResumer::new(config.clone());
    match resumer.discover_sessions() {
        Ok(report) => report,
        Err(error) => {
            warn!(
                session_resume = true,
                error = %error,
                "session discovery failed open with explicit incompleteness evidence"
            );
            SessionDiscoveryResult::fail_open_from_error(&error)
        }
    }
}

/// Return the native Antigravity conversations directory for a home directory.
pub fn antigravity_conversations_dir(home_dir: &Path) -> PathBuf {
    ANTIGRAVITY_CONVERSATIONS_RELATIVE_DIR
        .iter()
        .fold(home_dir.to_path_buf(), |path, component| {
            path.join(component)
        })
}

fn select_session_resume_home(
    home: Option<std::ffi::OsString>,
    user_profile: Option<std::ffi::OsString>,
    windows: bool,
) -> Option<PathBuf> {
    home.filter(|value| !value.is_empty())
        .or_else(|| {
            windows
                .then_some(user_profile)
                .flatten()
                .filter(|value| !value.is_empty())
        })
        .map(PathBuf::from)
}

/// Resolve the process home used by session discovery and CASR invocation.
///
/// `HOME` remains the primary cross-platform override. On Windows,
/// `USERPROFILE` is the platform-native fallback so a process without `HOME`
/// cannot accidentally turn an unavailable native inventory into an
/// authoritative empty one. `dirs::home_dir` is retained as the final
/// platform fallback for launch environments that expose neither variable.
#[must_use]
pub fn session_resume_home_dir() -> Option<PathBuf> {
    select_session_resume_home(
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
        cfg!(windows),
    )
    .or_else(dirs::home_dir)
}

/// Discover native Antigravity conversations under a testable home directory.
///
/// The Antigravity CLI stores one SQLite database per conversation at
/// `~/.gemini/antigravity-cli/conversations/<uuid>.db`; that filename stem is
/// the id accepted by `agy --conversation <uuid>`.
pub fn discover_antigravity_conversations_from_home(
    home_dir: &Path,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    validate_session_resume_home(home_dir)?;
    discover_antigravity_conversations_from_home_with_checkpoint(home_dir, || Ok(()))
}

/// Discover native Antigravity conversations under an explicit conversations dir.
pub fn discover_antigravity_conversations_in_dir(
    conversations_dir: &Path,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    discover_antigravity_conversations_in_dir_with_checkpoint(conversations_dir, || Ok(()))
}

fn open_session_directory_tree_nofollow(path: &Path) -> std::io::Result<cap_std::fs::Dir> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(std::io::Error::other(
            "session directory path contains a parent component",
        ));
    }
    let Some(leaf) = path.file_name() else {
        let base = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        return cap_std::fs::Dir::open_ambient_dir(base, cap_std::ambient_authority());
    };
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = open_session_directory_tree_nofollow(parent_path)?;
    open_session_child_directory_nofollow(&parent, Path::new(leaf))
}

fn open_session_child_directory_nofollow(
    parent: &cap_std::fs::Dir,
    component: &Path,
) -> std::io::Result<cap_std::fs::Dir> {
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let directory = parent.open_with(component, &options)?;
    if !directory.metadata()?.is_dir() {
        return Err(std::io::Error::other(
            "session directory component is not a directory",
        ));
    }
    Ok(cap_std::fs::Dir::from_std_file(directory.into_std()))
}

fn incomplete_native_directory(reason: SessionDiscoveryIncompleteReason) -> SessionDiscoveryResult {
    let mut report = SessionDiscoveryResult::default();
    report.mark_incomplete(SessionDiscoverySource::NativeAntigravity, reason);
    report
}

fn discover_antigravity_conversations_from_home_with_checkpoint(
    home_dir: &Path,
    mut checkpoint: impl FnMut() -> Result<(), SessionResumeError>,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    checkpoint()?;
    let mut resource_budget = NativeDiscoveryResourceBudget::for_root(home_dir)?;
    let mut directory = match open_session_directory_tree_nofollow(home_dir) {
        Ok(directory) => directory,
        Err(error) => {
            let reason = if error.kind() == std::io::ErrorKind::NotFound {
                SessionDiscoveryIncompleteReason::Unavailable
            } else {
                SessionDiscoveryIncompleteReason::DirectoryUnreadable
            };
            return Ok(incomplete_native_directory(reason));
        }
    };
    for component in ANTIGRAVITY_CONVERSATIONS_RELATIVE_DIR {
        checkpoint()?;
        resource_budget.charge_fixed_child(component)?;
        directory = match open_session_child_directory_nofollow(&directory, Path::new(component)) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // The caller-provided home was proven to be a real directory;
                // a missing fixed descendant therefore proves that there are
                // no native Antigravity conversations in that home.
                return Ok(SessionDiscoveryResult::default());
            }
            Err(_) => {
                return Ok(incomplete_native_directory(
                    SessionDiscoveryIncompleteReason::DirectoryUnreadable,
                ));
            }
        };
    }

    discover_antigravity_conversations_in_open_dir_with_checkpoint(
        directory,
        &antigravity_conversations_dir(home_dir),
        &mut resource_budget,
        checkpoint,
    )
}

fn discover_antigravity_conversations_in_dir_with_checkpoint(
    conversations_dir: &Path,
    mut checkpoint: impl FnMut() -> Result<(), SessionResumeError>,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    checkpoint()?;
    let mut resource_budget = NativeDiscoveryResourceBudget::for_root(conversations_dir)?;
    let directory = match open_session_directory_tree_nofollow(conversations_dir) {
        Ok(directory) => directory,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionDiscoveryResult::default());
        }
        Err(err) => {
            warn!(
                session_resume = true,
                provider = "agy",
                error_kind = ?err.kind(),
                "failed to open Antigravity conversations directory"
            );
            let mut report = SessionDiscoveryResult::default();
            report.mark_incomplete(
                SessionDiscoverySource::NativeAntigravity,
                SessionDiscoveryIncompleteReason::DirectoryUnreadable,
            );
            return Ok(report);
        }
    };
    discover_antigravity_conversations_in_open_dir_with_checkpoint(
        directory,
        conversations_dir,
        &mut resource_budget,
        checkpoint,
    )
}

fn discover_antigravity_conversations_in_open_dir_with_checkpoint(
    directory: cap_std::fs::Dir,
    conversations_dir: &Path,
    resource_budget: &mut NativeDiscoveryResourceBudget,
    mut checkpoint: impl FnMut() -> Result<(), SessionResumeError>,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    checkpoint()?;
    resource_budget.charge_syscalls(1)?;
    let dir_entries = match directory.entries() {
        Ok(entries) => entries,
        Err(err) => {
            warn!(
                session_resume = true,
                provider = "agy",
                error_kind = ?err.kind(),
                "failed to enumerate Antigravity conversations directory"
            );
            let mut report = SessionDiscoveryResult::default();
            report.mark_incomplete(
                SessionDiscoverySource::NativeAntigravity,
                SessionDiscoveryIncompleteReason::DirectoryUnreadable,
            );
            return Ok(report);
        }
    };

    let mut report = SessionDiscoveryResult::default();
    let mut unreadable_entry_count = 0_usize;
    let mut invalid_candidate_count = 0_usize;
    for (entry_index, dir_entry) in dir_entries.enumerate() {
        checkpoint()?;
        if entry_index >= MAX_SESSION_DISCOVERY_ENTRIES {
            return Err(SessionResumeError::DiscoveryLimitExceeded {
                source: SessionDiscoverySource::NativeAntigravity,
                limit: MAX_SESSION_DISCOVERY_ENTRIES,
            });
        }
        let dir_entry = match dir_entry {
            Ok(entry) => entry,
            Err(_) => {
                resource_budget.charge_syscalls(1)?;
                unreadable_entry_count = unreadable_entry_count.saturating_add(1);
                report.mark_incomplete(
                    SessionDiscoverySource::NativeAntigravity,
                    SessionDiscoveryIncompleteReason::DirectoryEntryUnreadable,
                );
                continue;
            }
        };
        let file_name = dir_entry.file_name();
        resource_budget.charge_directory_entry(&file_name)?;
        let relative_path = Path::new(&file_name);
        if relative_path.extension().and_then(|ext| ext.to_str()) != Some("db") {
            continue;
        }
        let Some(session_id) = relative_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| is_valid_antigravity_conversation_id(stem))
            .map(str::to_string)
        else {
            // A non-canonical filename cannot denote an Antigravity session.
            // Ignore it before applying fail-closed evidence to its file type;
            // otherwise an unrelated `notes.db` symlink could invalidate every
            // absence claim while an ordinary `notes.db` file remained benign.
            invalid_candidate_count = invalid_candidate_count.saturating_add(1);
            continue;
        };
        resource_budget.charge_candidate_metadata()?;
        checkpoint()?;
        let file_type = match dir_entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                unreadable_entry_count = unreadable_entry_count.saturating_add(1);
                report.mark_incomplete(
                    SessionDiscoverySource::NativeAntigravity,
                    SessionDiscoveryIncompleteReason::DirectoryEntryUnreadable,
                );
                continue;
            }
        };
        if file_type.is_symlink() {
            invalid_candidate_count = invalid_candidate_count.saturating_add(1);
            report.mark_incomplete(
                SessionDiscoverySource::NativeAntigravity,
                SessionDiscoveryIncompleteReason::SymlinkRejected,
            );
            continue;
        }
        if !file_type.is_file() {
            invalid_candidate_count = invalid_candidate_count.saturating_add(1);
            continue;
        }
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        checkpoint()?;
        let mut file = match directory.open_with(relative_path, &options) {
            Ok(file) => file,
            Err(_) => {
                unreadable_entry_count = unreadable_entry_count.saturating_add(1);
                report.mark_incomplete(
                    SessionDiscoverySource::NativeAntigravity,
                    SessionDiscoveryIncompleteReason::DirectoryEntryUnreadable,
                );
                continue;
            }
        };
        checkpoint()?;
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                unreadable_entry_count = unreadable_entry_count.saturating_add(1);
                report.mark_incomplete(
                    SessionDiscoverySource::NativeAntigravity,
                    SessionDiscoveryIncompleteReason::DirectoryEntryUnreadable,
                );
                continue;
            }
        };
        if !metadata.is_file() {
            invalid_candidate_count = invalid_candidate_count.saturating_add(1);
            continue;
        }
        let mut sqlite_header = [0_u8; 16];
        checkpoint()?;
        match file.read_exact(&mut sqlite_header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                invalid_candidate_count = invalid_candidate_count.saturating_add(1);
                continue;
            }
            Err(_) => {
                unreadable_entry_count = unreadable_entry_count.saturating_add(1);
                report.mark_incomplete(
                    SessionDiscoverySource::NativeAntigravity,
                    SessionDiscoveryIncompleteReason::DirectoryEntryUnreadable,
                );
                continue;
            }
        }
        if &sqlite_header != b"SQLite format 3\0" {
            invalid_candidate_count = invalid_candidate_count.saturating_add(1);
            continue;
        }

        let started_at = metadata
            .modified()
            .ok()
            .map(cap_std::time::SystemTime::into_std)
            .and_then(system_time_to_epoch_millis);
        checkpoint()?;
        let resume_plan = antigravity_native_resume_plan(&session_id)?;
        let result_path = conversations_dir.join(relative_path);
        resource_budget.charge_result_path(&result_path)?;
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "discovery_source".to_string(),
            serde_json::json!(ANTIGRAVITY_DISCOVERY_SOURCE),
        );
        extra.insert(
            "native_resume_command".to_string(),
            serde_json::json!(resume_plan.argv.clone()),
        );
        extra.insert(
            "native_resume_binary".to_string(),
            serde_json::json!(resume_plan.binary.clone()),
        );
        extra.insert(
            "provider_slug".to_string(),
            serde_json::json!(resume_plan.provider_slug.clone()),
        );
        extra.insert(
            "conversation_id".to_string(),
            serde_json::json!(resume_plan.session_id.clone()),
        );
        extra.insert(
            "model_name".to_string(),
            serde_json::json!(ANTIGRAVITY_MODEL),
        );
        extra.insert(
            "metadata_fallback_reason".to_string(),
            serde_json::json!(ANTIGRAVITY_METADATA_FALLBACK_REASON),
        );
        report.entries.push(CasrListEntry {
            session_id,
            provider: Some(AgentProvider::Antigravity.slug().to_string()),
            title: Some("Antigravity conversation (metadata schema not read)".to_string()),
            messages: 0,
            workspace: None,
            started_at,
            path: Some(result_path.display().to_string()),
            extra,
        });
    }

    checkpoint()?;

    if unreadable_entry_count > 0 {
        warn!(
            session_resume = true,
            provider = "agy",
            unreadable_entry_count,
            "Antigravity discovery skipped unreadable directory entries"
        );
    }
    if invalid_candidate_count > 0 {
        warn!(
            session_resume = true,
            provider = "agy",
            invalid_candidate_count,
            "Antigravity discovery ignored invalid conversation database candidates"
        );
    }
    report.entries.sort_by(|left, right| {
        left.session_id
            .cmp(&right.session_id)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(report)
}

/// Discover Antigravity conversations under the process HOME, if available.
pub fn discover_current_home_antigravity_conversations()
-> Result<SessionDiscoveryResult, SessionResumeError> {
    let Some(home) = session_resume_home_dir() else {
        return Ok(unavailable_native_discovery());
    };
    discover_antigravity_conversations_from_home(&home)
}

struct NativeDiscoveryPermit;

impl NativeDiscoveryPermit {
    fn try_acquire() -> Option<Self> {
        let previous = NATIVE_DISCOVERY_ACTIVE_SCANS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CONCURRENT_NATIVE_DISCOVERY_SCANS).then_some(active + 1)
            });
        let active = match previous {
            Ok(previous) => previous + 1,
            Err(_) => {
                saturating_increment(&NATIVE_DISCOVERY_SATURATED_TOTAL);
                return None;
            }
        };

        NATIVE_DISCOVERY_MAX_ACTIVE_SCANS.fetch_max(active, Ordering::Relaxed);
        saturating_increment(&NATIVE_DISCOVERY_ADMITTED_TOTAL);
        Some(Self)
    }
}

impl Drop for NativeDiscoveryPermit {
    fn drop(&mut self) {
        let previous = NATIVE_DISCOVERY_ACTIVE_SCANS.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "native discovery permit underflow");
    }
}

struct NativeDiscoveryWorkerGuard;

impl NativeDiscoveryWorkerGuard {
    fn new() -> Self {
        NATIVE_DISCOVERY_ACTIVE_WORKERS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for NativeDiscoveryWorkerGuard {
    fn drop(&mut self) {
        let previous = NATIVE_DISCOVERY_ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "native discovery worker counter underflow");
    }
}

#[derive(Clone)]
struct NativeDiscoveryCancellation {
    requested: Arc<AtomicBool>,
    runtime_shutdown: crate::runtime_async::RuntimeShutdownToken,
}

impl NativeDiscoveryCancellation {
    fn new(runtime_shutdown: crate::runtime_async::RuntimeShutdownToken) -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
            runtime_shutdown,
        }
    }

    fn request(&self) {
        if !self.requested.swap(true, Ordering::AcqRel) {
            saturating_increment(&NATIVE_DISCOVERY_CANCEL_REQUESTED_TOTAL);
        }
    }

    fn checkpoint(&self, cx: &crate::cx::Cx) -> Result<(), SessionResumeError> {
        if self.runtime_shutdown.is_shutdown_requested() {
            self.request();
            return Err(SessionResumeError::Cancelled);
        }
        if self.requested.load(Ordering::Acquire) {
            return Err(SessionResumeError::Cancelled);
        }
        if cx.checkpoint().is_err() {
            return Err(session_resume_cx_termination(cx));
        }
        Ok(())
    }
}

struct NativeDiscoveryObserverGuard {
    cancellation: NativeDiscoveryCancellation,
    request_on_drop: bool,
}

impl NativeDiscoveryObserverGuard {
    fn new(cancellation: NativeDiscoveryCancellation) -> Self {
        NATIVE_DISCOVERY_ACTIVE_OBSERVERS.fetch_add(1, Ordering::AcqRel);
        Self {
            cancellation,
            request_on_drop: true,
        }
    }

    fn request_cancellation(&mut self) {
        self.cancellation.request();
        self.request_on_drop = false;
    }

    fn disarm(&mut self) {
        self.request_on_drop = false;
    }
}

impl Drop for NativeDiscoveryObserverGuard {
    fn drop(&mut self) {
        if self.request_on_drop {
            self.cancellation.request();
            saturating_increment(&NATIVE_DISCOVERY_DROPPED_OBSERVER_TOTAL);
        }
        let previous = NATIVE_DISCOVERY_ACTIVE_OBSERVERS.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "native discovery observer counter underflow");
    }
}

enum NativeDiscoveryTerminalReceipt {
    Settled(Result<SessionDiscoveryResult, SessionResumeError>),
    WorkerFailed,
}

enum NativeDiscoveryCancellationWait {
    ContextEnded(SessionResumeError),
    TimerFailed,
}

fn classify_native_discovery_spawn_rejection(
    error: &crate::runtime_async::task::SpawnError,
) -> SessionDiscoveryAdmissionRejection {
    match error.code() {
        // Asupersync's stable lifecycle codes cover unavailable, closed, and
        // shutdown-racing regions. None admits filesystem work.
        "ASUP-E001" | "ASUP-E002" | "ASUP-E003" => {
            SessionDiscoveryAdmissionRejection::RuntimeUnavailableOrShuttingDown
        }
        "ASUP-E006" => SessionDiscoveryAdmissionRejection::RuntimeAtCapacity,
        _ => SessionDiscoveryAdmissionRejection::RuntimeRejected,
    }
}

fn native_discovery_owner_cx(caller_cx: &crate::cx::Cx) -> crate::cx::Cx {
    let caller_capabilities = crate::cx::effective_cap_mask(caller_cx);
    let _owner_context = crate::cx::Cx::set_current(Some(crate::cx::for_request()));
    let _caller_capability_ceiling = crate::cx::Cx::push_restriction(caller_capabilities);
    crate::cx::Cx::current().expect("fresh native-discovery owner context must be installed")
}

async fn wait_for_native_discovery_cancellation(
    cx: &crate::cx::Cx,
    cancellation: &NativeDiscoveryCancellation,
) -> NativeDiscoveryCancellationWait {
    loop {
        if cancellation.runtime_shutdown.is_shutdown_requested() {
            return NativeDiscoveryCancellationWait::ContextEnded(SessionResumeError::Cancelled);
        }
        if crate::runtime_async::sleep_with_cx(cx, SESSION_RESUME_CX_POLL_INTERVAL)
            .await
            .is_err()
        {
            return if cx.checkpoint().is_err() {
                NativeDiscoveryCancellationWait::ContextEnded(session_resume_cx_termination(cx))
            } else {
                NativeDiscoveryCancellationWait::TimerFailed
            };
        }
        if cx.checkpoint().is_err() {
            return NativeDiscoveryCancellationWait::ContextEnded(session_resume_cx_termination(
                cx,
            ));
        }
    }
}

async fn run_owned_native_discovery_with_cx<F>(
    cx: &crate::cx::Cx,
    work: F,
) -> Result<SessionDiscoveryResult, SessionResumeError>
where
    F: FnOnce(
            NativeDiscoveryCancellation,
            crate::cx::Cx,
        ) -> Result<SessionDiscoveryResult, SessionResumeError>
        + Send
        + 'static,
{
    cx.checkpoint()
        .map_err(|_| session_resume_cx_termination(cx))?;
    let Some(runtime_shutdown) = crate::runtime_async::current_runtime_shutdown_token() else {
        saturating_increment(&NATIVE_DISCOVERY_RUNTIME_REJECTED_TOTAL);
        return Err(SessionResumeError::DiscoveryAdmissionRejected {
            reason: SessionDiscoveryAdmissionRejection::RuntimeUnavailableOrShuttingDown,
        });
    };
    let Some(shutdown_lease) = runtime_shutdown.try_acquire() else {
        saturating_increment(&NATIVE_DISCOVERY_RUNTIME_REJECTED_TOTAL);
        return Err(SessionResumeError::DiscoveryAdmissionRejected {
            reason: SessionDiscoveryAdmissionRejection::RuntimeUnavailableOrShuttingDown,
        });
    };
    let permit = NativeDiscoveryPermit::try_acquire().ok_or(
        SessionResumeError::DiscoveryAdmissionRejected {
            reason: SessionDiscoveryAdmissionRejection::SubsystemSaturated,
        },
    )?;
    let cancellation = NativeDiscoveryCancellation::new(runtime_shutdown);
    let owner_cancellation = cancellation.clone();
    let scan_cx = cx.clone();
    let (receipt_tx, receipt_rx) = crate::runtime_async::oneshot::channel();

    // The owner context has an independent cancellation identity so caller
    // drop cannot abandon the blocking join, but it retains the caller's exact
    // effective capability ceiling. The scanner itself keeps the exact caller
    // Cx plus a private drop-cancellation signal. Runtime shutdown still owns
    // and drains this task through the region that accepted it.
    let owner_cx = native_discovery_owner_cx(cx);
    let owner = crate::runtime_async::task::try_spawn_with_cx(
        &owner_cx,
        move |_owner_cx| async move {
            let worker_cancellation = owner_cancellation.clone();
            let worker_scan_cx = scan_cx.clone();
            let worker = crate::runtime_async::spawn_blocking(move || {
                // The permit belongs to the blocking-work object, not merely
                // to the async supervisor. If runtime shutdown drops the
                // supervisor after this closure starts, the hard concurrency
                // ceiling must remain occupied until the closure settles.
                let _permit = permit;
                let _worker_guard = NativeDiscoveryWorkerGuard::new();
                worker_cancellation.checkpoint(&worker_scan_cx)?;
                work(worker_cancellation, worker_scan_cx)
            })
            .await;

            let receipt = match worker {
                Ok(result) => NativeDiscoveryTerminalReceipt::Settled(result),
                Err(_) => {
                    saturating_increment(&NATIVE_DISCOVERY_WORKER_FAILED_TOTAL);
                    NativeDiscoveryTerminalReceipt::WorkerFailed
                }
            };
            saturating_increment(&NATIVE_DISCOVERY_COMPLETED_TOTAL);
            let delivery_cx = crate::cx::for_request();
            if receipt_tx.send_with_cx(&delivery_cx, receipt).is_err() {
                saturating_increment(&NATIVE_DISCOVERY_UNDELIVERED_RECEIPT_TOTAL);
            }
            // Runtime::drop waits on this lease before tearing down the
            // scheduler. Release only after the one terminal receipt has been
            // delivered or conclusively found undeliverable.
            drop(shutdown_lease);
        },
    );
    let owner = match owner {
        Ok(owner) => owner,
        Err(error) => {
            saturating_increment(&NATIVE_DISCOVERY_RUNTIME_REJECTED_TOTAL);
            return Err(SessionResumeError::DiscoveryAdmissionRejected {
                reason: classify_native_discovery_spawn_rejection(&error),
            });
        }
    };
    // The accepted runtime region now owns the async supervisor. The caller
    // observes only the typed receipt, so dropping this request future cannot
    // drop the supervisor's blocking join.
    drop(owner);

    let cancellation_wait_signal = cancellation.clone();
    let mut observer = NativeDiscoveryObserverGuard::new(cancellation);
    let receipt_cx = crate::cx::for_request();
    let receipt = std::pin::pin!(crate::runtime_async::oneshot_recv_with_cx(
        &receipt_cx,
        receipt_rx
    ));
    let cancellation_wait = std::pin::pin!(wait_for_native_discovery_cancellation(
        cx,
        &cancellation_wait_signal
    ));
    use futures::future::{Either, select};
    match select(receipt, cancellation_wait).await {
        Either::Left((Ok(NativeDiscoveryTerminalReceipt::Settled(result)), _)) => {
            observer.disarm();
            result
        }
        Either::Left((Ok(NativeDiscoveryTerminalReceipt::WorkerFailed), _)) => {
            observer.disarm();
            Err(SessionResumeError::AsyncInfrastructureFailure)
        }
        Either::Left((Err(_), _)) => {
            // A vanished supervisor is not worker settlement. The blocking
            // closure may already be running, so request its private
            // cooperative cancellation before releasing the observer.
            observer.request_cancellation();
            Err(SessionResumeError::AsyncInfrastructureFailure)
        }
        Either::Right((NativeDiscoveryCancellationWait::ContextEnded(error), _)) => {
            observer.request_cancellation();
            Err(error)
        }
        Either::Right((NativeDiscoveryCancellationWait::TimerFailed, _)) => {
            observer.request_cancellation();
            Err(SessionResumeError::AsyncInfrastructureFailure)
        }
    }
}

/// Cx-first native Antigravity discovery under an explicit home directory.
/// The bounded filesystem scan runs on the canonical blocking executor under
/// a runtime-owned supervisor. Dropping the caller future requests private
/// cooperative cancellation while that supervisor retains the blocking join
/// through a typed terminal receipt. One already-started filesystem syscall is
/// the indivisible cooperative cancellation boundary.
pub async fn discover_antigravity_conversations_from_home_with_cx(
    cx: &crate::cx::Cx,
    home_dir: &Path,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    validate_session_resume_home(home_dir)?;
    // Reject caller-controlled path amplification before cloning the path into
    // the owned blocking-work closure. The scanner constructs the full budget
    // again so all later filesystem charges share one accumulator.
    NativeDiscoveryResourceBudget::for_root(home_dir)?;
    discover_antigravity_conversations_for_optional_home_with_cx(cx, Some(home_dir.to_path_buf()))
        .await
}

/// Cx-first native Antigravity discovery under the process HOME. This path
/// never invokes CASR.
pub async fn discover_current_home_antigravity_conversations_with_cx(
    cx: &crate::cx::Cx,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    let home = session_resume_home_dir();
    discover_antigravity_conversations_for_optional_home_with_cx(cx, home).await
}

async fn discover_antigravity_conversations_for_optional_home_with_cx(
    cx: &crate::cx::Cx,
    home_dir: Option<PathBuf>,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    let Some(home_dir) = home_dir else {
        cx.checkpoint()
            .map_err(|_| session_resume_cx_termination(cx))?;
        return Ok(unavailable_native_discovery());
    };
    run_owned_native_discovery_with_cx(cx, move |cancellation, scan_cx| {
        discover_antigravity_conversations_from_home_with_checkpoint(&home_dir, || {
            cancellation.checkpoint(&scan_cx)
        })
    })
    .await
}

fn unavailable_native_discovery() -> SessionDiscoveryResult {
    let mut report = SessionDiscoveryResult::default();
    report.mark_incomplete(
        SessionDiscoverySource::NativeAntigravity,
        SessionDiscoveryIncompleteReason::Unavailable,
    );
    report
}

fn validate_discovery_entry(entry: &CasrListEntry) -> Result<(), SessionResumeError> {
    validate_session_identifier(&entry.session_id)?;
    if let Some(provider) = entry.provider.as_deref() {
        validate_provider_slug(provider)?;
    }
    Ok(())
}

fn parse_casr_discovery_entries(output: &str) -> Result<Vec<CasrListEntry>, SessionResumeError> {
    let output_bytes = output.len();
    let entries: Vec<CasrListEntry> = serde_json::from_str(output)
        .map_err(|_| SessionResumeError::ParseError { output_bytes })?;
    if entries.len() > MAX_SESSION_DISCOVERY_ENTRIES {
        return Err(SessionResumeError::DiscoveryLimitExceeded {
            source: SessionDiscoverySource::Casr,
            limit: MAX_SESSION_DISCOVERY_ENTRIES,
        });
    }
    for entry in &entries {
        validate_discovery_entry(entry)?;
    }
    Ok(entries)
}

fn parse_casr_resume_output(output: &str) -> Result<CasrResumeOutput, SessionResumeError> {
    let output_bytes = output.len();
    let result: CasrResumeOutput = serde_json::from_str(output)
        .map_err(|_| SessionResumeError::ParseError { output_bytes })?;
    if !result.ok {
        warn!(session_resume = true, "resume reported failure");
        return Err(SessionResumeError::ResumeRejected);
    }
    Ok(result)
}

/// Merge a CASR inventory with an independently discovered native inventory.
/// CASR ordering wins, duplicates are removed by finite provider/session
/// identity, and admission is all-or-nothing when the combined cap is crossed.
pub fn merge_session_discovery_entries(
    native: &mut SessionDiscoveryResult,
    casr_entries: Vec<CasrListEntry>,
) -> Result<(), SessionResumeError> {
    if native.entries.len() > MAX_SESSION_DISCOVERY_ENTRIES {
        return Err(SessionResumeError::DiscoveryLimitExceeded {
            source: SessionDiscoverySource::NativeAntigravity,
            limit: MAX_SESSION_DISCOVERY_ENTRIES,
        });
    }
    if casr_entries.len() > MAX_SESSION_DISCOVERY_ENTRIES {
        return Err(SessionResumeError::DiscoveryLimitExceeded {
            source: SessionDiscoverySource::Casr,
            limit: MAX_SESSION_DISCOVERY_ENTRIES,
        });
    }

    for entry in native.entries.iter().chain(&casr_entries) {
        validate_discovery_entry(entry)?;
    }

    let capacity = native.entries.len().saturating_add(casr_entries.len());
    let mut seen = HashSet::with_capacity(capacity);
    let mut merged = Vec::with_capacity(capacity);
    for entry in casr_entries
        .into_iter()
        .chain(native.entries.iter().cloned())
    {
        let key = (
            provider_from_list_entry(&entry).slug().to_string(),
            entry.session_id.clone(),
        );
        if seen.insert(key) {
            if merged.len() == MAX_SESSION_DISCOVERY_ENTRIES {
                return Err(SessionResumeError::DiscoveryLimitExceeded {
                    source: SessionDiscoverySource::Merged,
                    limit: MAX_SESSION_DISCOVERY_ENTRIES,
                });
            }
            merged.push(entry);
        }
    }
    native.entries = merged;
    Ok(())
}

fn casr_discovery_incomplete_reason(
    error: &SessionResumeError,
) -> Option<SessionDiscoveryIncompleteReason> {
    match error {
        SessionResumeError::CasrNotFound => Some(SessionDiscoveryIncompleteReason::Unavailable),
        SessionResumeError::Timeout => Some(SessionDiscoveryIncompleteReason::TimedOut),
        SessionResumeError::SubprocessFailed { .. } => {
            Some(SessionDiscoveryIncompleteReason::SubprocessFailed)
        }
        SessionResumeError::WorkingDirectoryUnavailable
        | SessionResumeError::InvalidHomeDirectory => {
            Some(SessionDiscoveryIncompleteReason::InvalidConfiguration)
        }
        SessionResumeError::ParseError { .. }
        | SessionResumeError::ResumeRejected
        | SessionResumeError::InvalidSessionIdentifier { .. }
        | SessionResumeError::InvalidProviderSlug { .. } => {
            Some(SessionDiscoveryIncompleteReason::InvalidOutput)
        }
        SessionResumeError::CaptureLimitExceeded { .. } => {
            Some(SessionDiscoveryIncompleteReason::LimitExceeded)
        }
        SessionResumeError::CaptureIncomplete { .. } => {
            Some(SessionDiscoveryIncompleteReason::OutputCaptureIncomplete)
        }
        SessionResumeError::DiscoveryLimitExceeded {
            source: SessionDiscoverySource::Casr,
            ..
        } => Some(SessionDiscoveryIncompleteReason::LimitExceeded),
        SessionResumeError::DiscoveryLimitExceeded { .. }
        | SessionResumeError::DiscoveryResourceLimitExceeded { .. }
        | SessionResumeError::DiscoveryAdmissionRejected { .. }
        | SessionResumeError::DiscoveryIncomplete { .. }
        | SessionResumeError::InvalidOutputLimit { .. }
        | SessionResumeError::InvalidTimeout { .. }
        | SessionResumeError::AsyncInfrastructureFailure
        | SessionResumeError::Cancelled
        | SessionResumeError::CleanupIncomplete { .. }
        | SessionResumeError::SessionNotFound { .. }
        | SessionResumeError::ProviderNotInstalled
        | SessionResumeError::NativeProviderNotFound
        | SessionResumeError::NativeInteractiveTerminalRequired
        | SessionResumeError::InvalidNativeSessionId { .. }
        | SessionResumeError::NonPinnedNativeModel { .. }
        | SessionResumeError::ResumeEffectIndeterminate { .. } => None,
    }
}

fn discovery_error_incomplete_evidence(
    error: &SessionResumeError,
) -> (SessionDiscoverySource, SessionDiscoveryIncompleteReason) {
    match error {
        SessionResumeError::CasrNotFound => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::Unavailable,
        ),
        SessionResumeError::SubprocessFailed { .. } => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::SubprocessFailed,
        ),
        SessionResumeError::ParseError { .. } => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::InvalidOutput,
        ),
        SessionResumeError::ResumeRejected => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::SubprocessFailed,
        ),
        SessionResumeError::ResumeEffectIndeterminate { .. } => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::CleanupIncomplete,
        ),
        SessionResumeError::SessionNotFound { .. } => (
            SessionDiscoverySource::Merged,
            SessionDiscoveryIncompleteReason::RequestedTargetAbsent,
        ),
        SessionResumeError::ProviderNotInstalled => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::Unavailable,
        ),
        SessionResumeError::NativeProviderNotFound => (
            SessionDiscoverySource::NativeAntigravity,
            SessionDiscoveryIncompleteReason::Unavailable,
        ),
        SessionResumeError::NativeInteractiveTerminalRequired => (
            SessionDiscoverySource::NativeAntigravity,
            SessionDiscoveryIncompleteReason::InvalidConfiguration,
        ),
        SessionResumeError::InvalidNativeSessionId { .. }
        | SessionResumeError::NonPinnedNativeModel { .. } => (
            SessionDiscoverySource::NativeAntigravity,
            SessionDiscoveryIncompleteReason::InvalidConfiguration,
        ),
        SessionResumeError::InvalidSessionIdentifier { .. }
        | SessionResumeError::InvalidProviderSlug { .. } => (
            SessionDiscoverySource::Merged,
            SessionDiscoveryIncompleteReason::InvalidConfiguration,
        ),
        SessionResumeError::WorkingDirectoryUnavailable => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::InvalidConfiguration,
        ),
        SessionResumeError::InvalidHomeDirectory => (
            SessionDiscoverySource::Merged,
            SessionDiscoveryIncompleteReason::InvalidConfiguration,
        ),
        SessionResumeError::DiscoveryLimitExceeded { source, .. } => {
            (*source, SessionDiscoveryIncompleteReason::LimitExceeded)
        }
        SessionResumeError::DiscoveryResourceLimitExceeded { .. } => (
            SessionDiscoverySource::NativeAntigravity,
            SessionDiscoveryIncompleteReason::LimitExceeded,
        ),
        SessionResumeError::DiscoveryAdmissionRejected { .. } => (
            SessionDiscoverySource::NativeAntigravity,
            SessionDiscoveryIncompleteReason::AsyncInfrastructureFailure,
        ),
        SessionResumeError::InvalidOutputLimit { .. }
        | SessionResumeError::InvalidTimeout { .. } => (
            SessionDiscoverySource::Merged,
            SessionDiscoveryIncompleteReason::InvalidConfiguration,
        ),
        SessionResumeError::CaptureLimitExceeded { .. } => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::LimitExceeded,
        ),
        SessionResumeError::DiscoveryIncomplete { source, reason } => (*source, *reason),
        SessionResumeError::AsyncInfrastructureFailure => (
            SessionDiscoverySource::Merged,
            SessionDiscoveryIncompleteReason::AsyncInfrastructureFailure,
        ),
        SessionResumeError::Timeout => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::TimedOut,
        ),
        SessionResumeError::Cancelled => (
            SessionDiscoverySource::Merged,
            SessionDiscoveryIncompleteReason::Cancelled,
        ),
        SessionResumeError::CaptureIncomplete { .. } => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::OutputCaptureIncomplete,
        ),
        SessionResumeError::CleanupIncomplete { .. } => (
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::CleanupIncomplete,
        ),
    }
}

fn system_time_to_epoch_millis(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

/// Map a casr provider slug to an [`AgentProvider`].
pub fn provider_from_list_entry(entry: &CasrListEntry) -> AgentProvider {
    match &entry.provider {
        Some(slug) => AgentProvider::from_slug(slug),
        None => AgentProvider::Other("unknown".to_string()),
    }
}

/// Build a summary line for a list entry (for TUI/CLI display).
pub fn summarize_entry(entry: &CasrListEntry) -> String {
    let provider = crate::output::truncate_bounded(
        bounded_utf8_prefix(entry.provider.as_deref().unwrap_or("?"), 256),
        32,
        MAX_SESSION_RESUME_PROVIDER_BYTES,
    );
    let session_id = crate::output::truncate_bounded(
        bounded_utf8_prefix(&entry.session_id, MAX_SESSION_RESUME_ID_BYTES),
        80,
        MAX_SESSION_RESUME_ID_BYTES,
    );
    let title = crate::output::truncate_bounded(
        bounded_utf8_prefix(entry.title.as_deref().unwrap_or("(untitled)"), 1024),
        60,
        256,
    );
    let msgs = entry.messages;
    format!("[{}] {} ({} msgs) — {}", provider, session_id, msgs, title)
}

fn bounded_utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &value[..end]
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::casr_types::MessageRole;
    use crate::runtime_async::CompatRuntime;
    use serde_json::json;
    use std::collections::HashMap;

    #[derive(Default)]
    struct NativeDiscoveryTestBarrier {
        started: AtomicBool,
        released: std::sync::Mutex<bool>,
        changed: std::sync::Condvar,
    }

    impl NativeDiscoveryTestBarrier {
        fn wait_in_worker(&self) {
            self.started.store(true, Ordering::Release);
            let mut released = self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = self
                    .changed
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        fn release(&self) {
            *self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            self.changed.notify_all();
        }
    }

    struct ReleaseNativeDiscoveryBarrierOnDrop(Arc<NativeDiscoveryTestBarrier>);

    impl Drop for ReleaseNativeDiscoveryBarrierOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    struct NativeDiscoveryCheckpointGate {
        target: usize,
        reached: AtomicUsize,
        released: std::sync::Mutex<bool>,
        changed: std::sync::Condvar,
    }

    impl NativeDiscoveryCheckpointGate {
        fn new(target: usize) -> Self {
            Self {
                target,
                reached: AtomicUsize::new(0),
                released: std::sync::Mutex::new(false),
                changed: std::sync::Condvar::new(),
            }
        }

        fn checkpoint_in_worker(&self) {
            let stage = self.reached.fetch_add(1, Ordering::AcqRel) + 1;
            if stage != self.target {
                return;
            }
            let mut released = self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = self
                    .changed
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        fn release(&self) {
            *self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            self.changed.notify_all();
        }
    }

    struct ReleaseNativeDiscoveryCheckpointOnDrop(Arc<NativeDiscoveryCheckpointGate>);

    impl Drop for ReleaseNativeDiscoveryCheckpointOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    fn native_discovery_lifecycle_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn wait_for_native_discovery_state(
        cx: &crate::cx::Cx,
        predicate: impl Fn(NativeDiscoveryRuntimeMetrics) -> bool,
    ) {
        let started = std::time::Instant::now();
        loop {
            let snapshot = native_discovery_runtime_metrics();
            if predicate(snapshot) {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "native discovery lifecycle did not converge: {snapshot:?}"
            );
            crate::runtime_async::sleep_with_cx(cx, Duration::from_millis(1))
                .await
                .expect("test lifecycle poll must remain live");
        }
    }

    fn wait_for_native_discovery_state_blocking(
        predicate: impl Fn(NativeDiscoveryRuntimeMetrics) -> bool,
    ) {
        let started = std::time::Instant::now();
        loop {
            let snapshot = native_discovery_runtime_metrics();
            if predicate(snapshot) {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "native discovery blocking lifecycle did not converge: {snapshot:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // -- AgentProvider --

    #[test]
    fn agent_provider_slug_roundtrip() {
        let providers = vec![
            AgentProvider::ClaudeCode,
            AgentProvider::Codex,
            AgentProvider::Gemini,
            AgentProvider::Antigravity,
            AgentProvider::Grok,
            AgentProvider::Other("custom".into()),
        ];
        for p in providers {
            let slug = p.slug();
            let rt = AgentProvider::from_slug(slug);
            assert_eq!(p, rt);
        }
    }

    #[test]
    fn agent_provider_aliases() {
        assert_eq!(AgentProvider::from_slug("cc"), AgentProvider::ClaudeCode);
        assert_eq!(AgentProvider::from_slug("cod"), AgentProvider::Codex);
        assert_eq!(AgentProvider::from_slug("gmi"), AgentProvider::Gemini);
        assert_eq!(AgentProvider::from_slug(" Gemini "), AgentProvider::Gemini);
        assert_eq!(AgentProvider::from_slug("agy"), AgentProvider::Antigravity);
        assert_eq!(
            AgentProvider::from_slug("antigravity"),
            AgentProvider::Antigravity
        );
        assert_eq!(
            AgentProvider::from_slug(" Antigravity-CLI "),
            AgentProvider::Antigravity
        );
    }

    #[test]
    fn antigravity_native_resume_plan_is_model_pinned_and_serializable() {
        let conversation_id = "123e4567-e89b-12d3-a456-426614174000";
        let plan = antigravity_native_resume_plan(conversation_id).unwrap();

        assert_eq!(plan.provider_slug, "agy");
        assert_eq!(plan.session_id, conversation_id);
        assert_eq!(plan.binary, ANTIGRAVITY_BINARY);
        assert_eq!(plan.model_name.as_deref(), Some(ANTIGRAVITY_MODEL));
        assert_eq!(
            plan.argv,
            vec![
                "agy".to_string(),
                "--conversation".to_string(),
                conversation_id.to_string(),
                "--model".to_string(),
                ANTIGRAVITY_MODEL.to_string(),
            ]
        );

        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["provider_slug"], "agy");
        assert_eq!(json["model_name"], ANTIGRAVITY_MODEL);
        assert_eq!(json["argv"][0], "agy");
    }

    #[test]
    fn antigravity_native_resume_plan_rejects_bad_ids_and_model_overrides() {
        let bad_id = "../123e4567-e89b-12d3-a456-426614174000";
        let err = antigravity_native_resume_plan(bad_id).unwrap_err();
        assert!(matches!(
            &err,
            SessionResumeError::InvalidNativeSessionId {
                input_bytes,
                reason: NativeSessionIdInvalidReason::WrongShape,
            } if *input_bytes == bad_id.len()
        ));
        assert!(!err.to_string().contains(bad_id));

        let err = antigravity_native_resume_plan_with_model(
            "123e4567-e89b-12d3-a456-426614174000",
            "Gemini 3.1 Pro",
        )
        .unwrap_err();
        assert!(matches!(
            &err,
            SessionResumeError::NonPinnedNativeModel {
                requested_model_bytes,
            } if *requested_model_bytes == "Gemini 3.1 Pro".len()
        ));
        assert!(!err.to_string().contains("Gemini 3.1 Pro"));
    }

    #[test]
    fn antigravity_native_resume_plan_reports_missing_binary_provider_specifically() {
        let plan = antigravity_native_resume_plan("123e4567-e89b-12d3-a456-426614174000").unwrap();
        let empty_path = tempfile::tempdir().expect("temp empty path");
        let err = plan
            .require_binary_available_in_path(Some(empty_path.path().to_str().unwrap()))
            .unwrap_err();

        assert_eq!(err, SessionResumeError::NativeProviderNotFound);
        assert_eq!(err.to_string(), "native provider binary unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_native_resume_plan_rejects_non_executable_path_entry() {
        use std::os::unix::fs::PermissionsExt;

        let plan = antigravity_native_resume_plan("123e4567-e89b-12d3-a456-426614174000").unwrap();
        let path_dir = tempfile::tempdir().expect("temporary PATH directory");
        let candidate = path_dir.path().join(ANTIGRAVITY_BINARY);
        fs::write(&candidate, b"not executable").expect("write non-executable PATH fixture");
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644))
            .expect("mark PATH fixture non-executable");

        let error = plan
            .require_binary_available_in_path(Some(path_dir.path().to_str().unwrap()))
            .expect_err("a regular file without execute permission is not an available binary");
        assert_eq!(error, SessionResumeError::NativeProviderNotFound);
    }

    #[test]
    fn antigravity_conversation_id_validation_is_uuid_only() {
        assert!(is_valid_antigravity_conversation_id(
            "123e4567-e89b-12d3-a456-426614174000"
        ));
        assert!(is_valid_antigravity_conversation_id(
            "123E4567-E89B-12D3-A456-426614174000"
        ));
        assert!(!is_valid_antigravity_conversation_id("session-legacy"));
        assert!(!is_valid_antigravity_conversation_id(
            "123e4567-e89b-12d3-a456-426614174000/extra"
        ));
        assert!(!is_valid_antigravity_conversation_id(
            "123e4567-e89b-12d3-a456-42661417400z"
        ));
    }

    #[test]
    fn session_resume_home_selection_uses_userprofile_on_windows_without_home() {
        let selected = select_session_resume_home(
            None,
            Some(std::ffi::OsString::from(r"C:\Users\operator")),
            true,
        );
        assert_eq!(selected, Some(PathBuf::from(r"C:\Users\operator")));
        assert_eq!(
            select_session_resume_home(
                Some(std::ffi::OsString::from("/explicit-home")),
                Some(std::ffi::OsString::from(r"C:\Users\operator")),
                true,
            ),
            Some(PathBuf::from("/explicit-home")),
            "HOME remains the explicit cross-platform override",
        );
        assert_eq!(
            select_session_resume_home(
                None,
                Some(std::ffi::OsString::from(r"C:\Users\operator")),
                false,
            ),
            None,
            "USERPROFILE must not silently redefine HOME on non-Windows hosts",
        );
    }

    #[test]
    fn missing_explicit_home_cannot_authorize_native_session_absence() {
        let parent = tempfile::tempdir().expect("isolated missing-home parent");
        let missing_home = parent.path().join("home-does-not-exist");
        let report = discover_antigravity_conversations_from_home(&missing_home)
            .expect("missing home is represented as typed incomplete evidence");
        assert!(report.entries.is_empty());
        assert_eq!(
            report.require_complete_for_absence_claim(),
            Err(SessionResumeError::DiscoveryIncomplete {
                source: SessionDiscoverySource::NativeAntigravity,
                reason: SessionDiscoveryIncompleteReason::Unavailable,
            })
        );
    }

    #[test]
    fn existing_home_without_conversations_is_authoritative_empty() {
        let home = tempfile::tempdir().expect("isolated existing home");
        let report = discover_antigravity_conversations_from_home(home.path())
            .expect("missing fixed descendants mean no native conversations");
        assert!(report.entries.is_empty());
        assert!(report.is_complete());
        assert_eq!(report.require_complete_for_absence_claim(), Ok(()));
    }

    #[test]
    fn native_discovery_root_path_budgets_fail_before_filesystem_effects() {
        let oversized = PathBuf::from(format!(
            "/{}",
            "a".repeat(MAX_NATIVE_DISCOVERY_HOME_PATH_BYTES)
        ));
        assert_eq!(
            NativeDiscoveryResourceBudget::for_root(&oversized),
            Err(SessionResumeError::DiscoveryResourceLimitExceeded {
                resource: SessionDiscoveryResource::HomePathBytes,
                observed: MAX_NATIVE_DISCOVERY_HOME_PATH_BYTES + 1,
                limit: MAX_NATIVE_DISCOVERY_HOME_PATH_BYTES,
            })
        );

        let too_many_components = PathBuf::from(format!(
            "/{}",
            std::iter::repeat_n("a", MAX_NATIVE_DISCOVERY_HOME_COMPONENTS)
                .collect::<Vec<_>>()
                .join("/")
        ));
        assert_eq!(
            NativeDiscoveryResourceBudget::for_root(&too_many_components),
            Err(SessionResumeError::DiscoveryResourceLimitExceeded {
                resource: SessionDiscoveryResource::HomePathComponents,
                observed: MAX_NATIVE_DISCOVERY_HOME_COMPONENTS + 1,
                limit: MAX_NATIVE_DISCOVERY_HOME_COMPONENTS,
            })
        );
    }

    #[test]
    fn explicit_native_discovery_rejects_oversized_home_before_runtime_admission() {
        use std::future::Future as _;

        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        let oversized = PathBuf::from(format!(
            "/{}",
            "a".repeat(MAX_NATIVE_DISCOVERY_HOME_PATH_BYTES)
        ));
        let cx = crate::cx::for_request();
        let mut request = Box::pin(discover_antigravity_conversations_from_home_with_cx(
            &cx, &oversized,
        ));
        let waker = futures::task::noop_waker();
        let mut poll_cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            request.as_mut().poll(&mut poll_cx),
            std::task::Poll::Ready(Err(
                SessionResumeError::DiscoveryResourceLimitExceeded {
                    resource: SessionDiscoveryResource::HomePathBytes,
                    observed,
                    limit: MAX_NATIVE_DISCOVERY_HOME_PATH_BYTES,
                }
            )) if observed == MAX_NATIVE_DISCOVERY_HOME_PATH_BYTES + 1
        ));
        assert_eq!(
            native_discovery_runtime_metrics(),
            before,
            "root preflight must not acquire runtime ownership"
        );
    }

    #[test]
    fn native_discovery_charged_resources_stop_at_exact_limits() {
        let mut names = NativeDiscoveryResourceBudget::default();
        names.entry_name_bytes = MAX_NATIVE_DISCOVERY_ENTRY_NAME_BYTES;
        assert_eq!(
            names.charge_entry_name_bytes(1),
            Err(SessionResumeError::DiscoveryResourceLimitExceeded {
                resource: SessionDiscoveryResource::EntryNameBytes,
                observed: MAX_NATIVE_DISCOVERY_ENTRY_NAME_BYTES + 1,
                limit: MAX_NATIVE_DISCOVERY_ENTRY_NAME_BYTES,
            })
        );

        let mut result_paths = NativeDiscoveryResourceBudget::default();
        result_paths.result_path_bytes = MAX_NATIVE_DISCOVERY_RESULT_PATH_BYTES;
        assert_eq!(
            result_paths.charge_result_path(Path::new("x")),
            Err(SessionResumeError::DiscoveryResourceLimitExceeded {
                resource: SessionDiscoveryResource::ResultPathBytes,
                observed: MAX_NATIVE_DISCOVERY_RESULT_PATH_BYTES + 1,
                limit: MAX_NATIVE_DISCOVERY_RESULT_PATH_BYTES,
            })
        );

        let mut metadata = NativeDiscoveryResourceBudget::default();
        metadata.metadata_bytes = MAX_NATIVE_DISCOVERY_METADATA_BYTES;
        assert_eq!(
            metadata.charge_candidate_metadata(),
            Err(SessionResumeError::DiscoveryResourceLimitExceeded {
                resource: SessionDiscoveryResource::MetadataBytes,
                observed: MAX_NATIVE_DISCOVERY_METADATA_BYTES
                    + NATIVE_DISCOVERY_METADATA_CHARGE_PER_ENTRY,
                limit: MAX_NATIVE_DISCOVERY_METADATA_BYTES,
            })
        );

        let mut syscalls = NativeDiscoveryResourceBudget::default();
        syscalls.syscalls = MAX_NATIVE_DISCOVERY_SYSCALLS;
        assert_eq!(
            syscalls.charge_syscalls(1),
            Err(SessionResumeError::DiscoveryResourceLimitExceeded {
                resource: SessionDiscoveryResource::Syscalls,
                observed: MAX_NATIVE_DISCOVERY_SYSCALLS + 1,
                limit: MAX_NATIVE_DISCOVERY_SYSCALLS,
            })
        );
    }

    #[test]
    fn unavailable_native_discovery_is_never_authoritative_empty() {
        let report = unavailable_native_discovery();
        assert!(report.entries.is_empty());
        assert!(!report.is_complete());
        assert!(report.require_complete_for_absence_claim().is_err());
    }

    #[test]
    fn agent_provider_unknown_slug() {
        let p = AgentProvider::from_slug("future-agent");
        assert_eq!(p, AgentProvider::Other("future-agent".into()));
        assert_eq!(p.slug(), "future-agent");
    }

    #[test]
    fn agent_provider_display() {
        assert_eq!(AgentProvider::ClaudeCode.to_string(), "claude-code");
        assert_eq!(AgentProvider::Codex.to_string(), "codex");
    }

    #[test]
    fn agent_provider_serde_roundtrip() {
        let p = AgentProvider::ClaudeCode;
        let json_str = serde_json::to_string(&p).unwrap();
        let rt: AgentProvider = serde_json::from_str(&json_str).unwrap();
        assert_eq!(p, rt);
    }

    #[test]
    fn agent_provider_antigravity_serde_uses_agy_slug() {
        let json_str = serde_json::to_string(&AgentProvider::Antigravity).unwrap();
        assert_eq!(json_str, "\"agy\"");
        assert_eq!(
            serde_json::from_str::<AgentProvider>("\"antigravity\"").unwrap(),
            AgentProvider::Antigravity
        );
        assert_eq!(
            serde_json::from_str::<AgentProvider>("\"antigravity-cli\"").unwrap(),
            AgentProvider::Antigravity
        );
    }

    #[test]
    fn agent_provider_other_serde_roundtrip() {
        let p = AgentProvider::Other("custom-x".into());
        let json_str = serde_json::to_string(&p).unwrap();
        let rt: AgentProvider = serde_json::from_str(&json_str).unwrap();
        assert_eq!(p, rt);
    }

    // -- SessionResumeConfig --

    #[test]
    fn config_default() {
        let c = SessionResumeConfig::default();
        assert_eq!(c.casr_binary, "casr");
        assert_eq!(c.timeout_secs, 30);
        assert!(!c.dry_run);
        assert!(c.working_dir.is_none());
        assert!(c.home_dir.is_none());
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = SessionResumeConfig {
            casr_binary: "/usr/bin/casr".into(),
            working_dir: Some(PathBuf::from("/project")),
            home_dir: Some(PathBuf::from("/home/operator")),
            timeout_secs: 60,
            dry_run: true,
        };
        let json_str = serde_json::to_string(&c).unwrap();
        let rt: SessionResumeConfig = serde_json::from_str(&json_str).unwrap();
        assert_eq!(rt.casr_binary, "/usr/bin/casr");
        assert_eq!(rt.timeout_secs, 60);
        assert!(rt.dry_run);
    }

    #[test]
    fn config_serde_defaults() {
        let json_str = "{}";
        let c: SessionResumeConfig = serde_json::from_str(json_str).unwrap();
        assert_eq!(c.casr_binary, "casr");
        assert_eq!(c.timeout_secs, 30);
        assert!(!c.dry_run);
    }

    // -- SessionResumer --

    #[test]
    fn resumer_with_defaults() {
        let r = SessionResumer::with_defaults();
        assert_eq!(r.config().casr_binary, "casr");
    }

    #[test]
    fn resumer_casr_not_available() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        assert!(!r.is_casr_available());
    }

    #[test]
    fn resumer_discover_retains_typed_partial_report_when_binary_missing() {
        let home = tempfile::tempdir().expect("isolated native discovery home");
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let report = r
            .discover_sessions_in_home(home.path())
            .expect("native discovery remains usable");
        assert!(report.entries.is_empty());
        assert!(!report.is_complete());
        assert!(report.incomplete.contains(&SessionDiscoveryIncomplete {
            source: SessionDiscoverySource::Casr,
            reason: SessionDiscoveryIncompleteReason::Unavailable,
        }));
    }

    #[test]
    fn resumer_list_providers_fails_gracefully() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let result = r.list_providers();
        assert!(result.is_err());
    }

    #[test]
    fn resumer_resume_fails_gracefully() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let result = r.resume_session("sess-1", &AgentProvider::Codex);
        assert!(result.is_err());
    }

    // -- RecorderCasrExport --

    #[test]
    fn export_for_recorder_basic() {
        let r = SessionResumer::with_defaults();
        let messages = vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "hello".into(),
            timestamp: Some(1_700_000_000_000),
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({}),
        }];
        let export = r.export_for_recorder(
            "sess-1",
            "claude-code",
            Path::new("/tmp/src.jsonl"),
            messages,
            vec![1, 2],
        );
        assert_eq!(export.session.session_id, "sess-1");
        assert_eq!(export.pane_ids, vec![1, 2]);
        assert_eq!(export.events_processed, 1);
        assert!(export.warnings.is_empty());
    }

    #[test]
    fn export_for_recorder_empty_messages() {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder("sess-2", "codex", Path::new("/tmp/x"), vec![], vec![]);
        assert_eq!(export.events_processed, 0);
        assert!(export.session.started_at.is_none());
        assert!(export.session.ended_at.is_none());
    }

    #[test]
    fn export_for_recorder_timestamps() {
        let r = SessionResumer::with_defaults();
        let messages = vec![
            CanonicalMessage {
                idx: 0,
                role: MessageRole::User,
                content: "a".into(),
                timestamp: Some(100),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            },
            CanonicalMessage {
                idx: 1,
                role: MessageRole::Assistant,
                content: "b".into(),
                timestamp: Some(200),
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            },
        ];
        let export = r.export_for_recorder("s", "cc", Path::new("/x"), messages, vec![]);
        assert_eq!(export.session.started_at, Some(100));
        assert_eq!(export.session.ended_at, Some(200));
    }

    #[test]
    fn export_serde_roundtrip() {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder("s1", "codex", Path::new("/tmp/x"), vec![], vec![42]);
        let json_str = serde_json::to_string(&export).unwrap();
        let rt: RecorderCasrExport = serde_json::from_str(&json_str).unwrap();
        assert_eq!(rt.session.session_id, "s1");
        assert_eq!(rt.pane_ids, vec![42]);
    }

    // -- SessionResumeError --

    #[test]
    fn error_display() {
        let e = SessionResumeError::CasrNotFound;
        assert!(e.to_string().contains("unavailable"));

        let e = SessionResumeError::SubprocessFailed { code: Some(1) };
        assert!(e.to_string().contains("exit 1"));

        let e = SessionResumeError::ParseError { output_bytes: 8 };
        assert!(e.to_string().contains("8 bytes"));

        let e = SessionResumeError::ResumeRejected;
        assert_eq!(e.to_string(), "casr reported that resume failed");

        let e = SessionResumeError::SessionNotFound {
            identifier_bytes: 3,
        };
        assert!(e.to_string().contains("identifier_bytes=3"));

        let e = SessionResumeError::ProviderNotInstalled;
        assert_eq!(e.to_string(), "provider not installed");

        let e = SessionResumeError::NativeProviderNotFound;
        assert_eq!(e.to_string(), "native provider binary unavailable");

        let e = SessionResumeError::InvalidNativeSessionId {
            input_bytes: 6,
            reason: NativeSessionIdInvalidReason::WrongShape,
        };
        assert!(e.to_string().contains("input_bytes=6"));

        let e = SessionResumeError::NonPinnedNativeModel {
            requested_model_bytes: 14,
        };
        assert!(e.to_string().contains("requested_bytes=14"));

        let e = SessionResumeError::Timeout;
        assert!(e.to_string().contains("timed out"));

        let e = SessionResumeError::Cancelled;
        assert!(e.to_string().contains("cancelled"));

        let e = SessionResumeError::CaptureIncomplete {
            stdout_open: true,
            stderr_open: false,
            drain_timeout_ms: 100,
        };
        assert!(e.to_string().contains("capture incomplete"));

        let e = SessionResumeError::CleanupIncomplete {
            trigger: CommandCleanupTrigger::TimedOut,
            leader_reaped: false,
            signal_helper_settled: true,
            process_tree_signalled: false,
            stdout_open: false,
            stderr_open: false,
            settle_timeout_ms: 250,
        };
        assert!(e.to_string().contains("process cleanup incomplete"));
    }

    #[test]
    fn error_is_std_error() {
        let e: Box<dyn std::error::Error> = Box::new(SessionResumeError::Cancelled);
        assert!(!e.to_string().is_empty());
    }

    // -- Helper functions --

    #[test]
    fn failopen_conversion_marks_empty_inventory_incomplete_without_execution() {
        let result =
            SessionDiscoveryResult::fail_open_from_error(&SessionResumeError::CasrNotFound);
        assert!(result.entries.is_empty());
        assert!(!result.is_complete());
        assert!(result.incomplete.contains(&SessionDiscoveryIncomplete {
            source: SessionDiscoverySource::Casr,
            reason: SessionDiscoveryIncompleteReason::Unavailable,
        }));
    }

    #[test]
    fn failopen_conversion_preserves_finite_failure_evidence() {
        let cases = [
            (
                SessionResumeError::Cancelled,
                SessionDiscoverySource::Merged,
                SessionDiscoveryIncompleteReason::Cancelled,
            ),
            (
                SessionResumeError::AsyncInfrastructureFailure,
                SessionDiscoverySource::Merged,
                SessionDiscoveryIncompleteReason::AsyncInfrastructureFailure,
            ),
            (
                SessionResumeError::CleanupIncomplete {
                    trigger: CommandCleanupTrigger::TimedOut,
                    leader_reaped: false,
                    signal_helper_settled: true,
                    process_tree_signalled: true,
                    stdout_open: false,
                    stderr_open: false,
                    settle_timeout_ms: 250,
                },
                SessionDiscoverySource::Casr,
                SessionDiscoveryIncompleteReason::CleanupIncomplete,
            ),
            (
                SessionResumeError::InvalidProviderSlug { input_bytes: 17 },
                SessionDiscoverySource::Merged,
                SessionDiscoveryIncompleteReason::InvalidConfiguration,
            ),
            (
                SessionResumeError::DiscoveryIncomplete {
                    source: SessionDiscoverySource::NativeAntigravity,
                    reason: SessionDiscoveryIncompleteReason::DirectoryUnreadable,
                },
                SessionDiscoverySource::NativeAntigravity,
                SessionDiscoveryIncompleteReason::DirectoryUnreadable,
            ),
        ];

        for (error, source, reason) in cases {
            let result = SessionDiscoveryResult::fail_open_from_error(&error);
            assert_eq!(
                result.incomplete,
                vec![SessionDiscoveryIncomplete { source, reason }]
            );
        }
    }

    #[test]
    fn partial_inventory_cannot_authorize_an_absence_claim() {
        let partial = SessionDiscoveryResult {
            entries: Vec::new(),
            incomplete: vec![SessionDiscoveryIncomplete {
                source: SessionDiscoverySource::Casr,
                reason: SessionDiscoveryIncompleteReason::TimedOut,
            }],
        };
        assert_eq!(
            partial.require_complete_for_absence_claim(),
            Err(SessionResumeError::DiscoveryIncomplete {
                source: SessionDiscoverySource::Casr,
                reason: SessionDiscoveryIncompleteReason::TimedOut,
            })
        );
        assert_eq!(
            SessionDiscoveryResult::default().require_complete_for_absence_claim(),
            Ok(())
        );
    }

    #[test]
    fn public_identity_admission_is_content_free() {
        let session_canary = " secret-session ";
        let session_error = validate_session_identifier(session_canary)
            .expect_err("surrounding whitespace is rejected before discovery");
        assert_eq!(
            session_error,
            SessionResumeError::InvalidSessionIdentifier {
                input_bytes: session_canary.len(),
            }
        );
        assert!(!session_error.to_string().contains(session_canary));

        let provider_canary = "provider/../../canary";
        let provider_error = validate_provider_slug(provider_canary)
            .expect_err("path-like provider slugs are rejected before discovery");
        assert_eq!(
            provider_error,
            SessionResumeError::InvalidProviderSlug {
                input_bytes: provider_canary.len(),
            }
        );
        assert!(!provider_error.to_string().contains(provider_canary));
    }

    #[test]
    fn provider_from_list_entry_known() {
        let entry = CasrListEntry {
            session_id: "s1".into(),
            provider: Some("claude-code".into()),
            title: None,
            messages: 0,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        assert_eq!(provider_from_list_entry(&entry), AgentProvider::ClaudeCode);
    }

    #[test]
    fn provider_from_list_entry_none() {
        let entry = CasrListEntry {
            session_id: "s1".into(),
            provider: None,
            title: None,
            messages: 0,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        assert_eq!(
            provider_from_list_entry(&entry),
            AgentProvider::Other("unknown".into())
        );
    }

    #[test]
    fn summarize_entry_full() {
        let entry = CasrListEntry {
            session_id: "abc-123".into(),
            provider: Some("codex".into()),
            title: Some("Fix the bug".into()),
            messages: 42,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        let summary = summarize_entry(&entry);
        assert!(summary.contains("codex"));
        assert!(summary.contains("abc-123"));
        assert!(summary.contains("42 msgs"));
        assert!(summary.contains("Fix the bug"));
    }

    #[test]
    fn summarize_entry_missing_fields() {
        let entry = CasrListEntry {
            session_id: "s1".into(),
            provider: None,
            title: None,
            messages: 0,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        let summary = summarize_entry(&entry);
        assert!(summary.contains("?"));
        assert!(summary.contains("(untitled)"));
    }

    #[test]
    fn summarize_entry_long_title_truncated() {
        let long_title = "a".repeat(200);
        let entry = CasrListEntry {
            session_id: "s1".into(),
            provider: Some("cc".into()),
            title: Some(long_title),
            messages: 1,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        let summary = summarize_entry(&entry);
        // Title should be truncated to 60 chars
        assert!(summary.len() < 200);
    }

    #[test]
    fn summarize_entry_sanitizes_controls_and_bounds_input_work() {
        let entry = CasrListEntry {
            session_id: format!("id\u{1b}[31m{}", "x".repeat(10_000)),
            provider: Some(format!("codex\n{}", "p".repeat(10_000))),
            title: Some(format!("title\u{202e}{}", "t".repeat(10_000))),
            messages: 1,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        };
        let summary = summarize_entry(&entry);
        assert!(!summary.contains('\u{1b}'));
        assert!(!summary.contains('\n'));
        assert!(!summary.contains('\u{202e}'));
        assert!(summary.len() < 512);
    }

    // -- AgentProvider edge cases --

    #[test]
    fn agent_provider_all_variants_serializable() {
        let variants = vec![
            AgentProvider::ClaudeCode,
            AgentProvider::Codex,
            AgentProvider::Gemini,
            AgentProvider::Antigravity,
            AgentProvider::Grok,
            AgentProvider::Other("x".into()),
        ];
        for v in &variants {
            let json_str = serde_json::to_string(v).unwrap();
            assert!(!json_str.is_empty());
        }
    }

    #[test]
    fn agent_provider_kebab_case_serialization() {
        let json_str = serde_json::to_string(&AgentProvider::ClaudeCode).unwrap();
        assert!(json_str.contains("claude-code"));
    }

    #[test]
    fn agent_provider_hash_and_eq() {
        let mut set = std::collections::HashSet::new();
        set.insert(AgentProvider::ClaudeCode);
        set.insert(AgentProvider::ClaudeCode);
        assert_eq!(set.len(), 1);
        set.insert(AgentProvider::Codex);
        assert_eq!(set.len(), 2);
    }

    // -- Config edge cases --

    #[test]
    fn config_custom_binary_and_dir() {
        let c = SessionResumeConfig {
            casr_binary: "/opt/bin/casr".into(),
            working_dir: Some(PathBuf::from("/my/project")),
            home_dir: Some(PathBuf::from("/my/home")),
            timeout_secs: 120,
            dry_run: true,
        };
        let r = SessionResumer::new(c);
        assert_eq!(r.config().casr_binary, "/opt/bin/casr");
        assert_eq!(
            r.config().working_dir.as_deref(),
            Some(Path::new("/my/project"))
        );
        assert_eq!(r.config().home_dir.as_deref(), Some(Path::new("/my/home")));
    }

    #[test]
    fn resumer_is_provider_installed_fails_gracefully() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let result = r.is_provider_installed(&AgentProvider::ClaudeCode);
        assert!(result.is_err());
    }

    #[test]
    fn provider_filter_retains_only_relevant_absence_authority() {
        let mut base = SessionDiscoveryResult::default();
        base.mark_incomplete(
            SessionDiscoverySource::Casr,
            SessionDiscoveryIncompleteReason::Unavailable,
        );
        base.mark_incomplete(
            SessionDiscoverySource::NativeAntigravity,
            SessionDiscoveryIncompleteReason::DirectoryUnreadable,
        );
        base.mark_incomplete(
            SessionDiscoverySource::Merged,
            SessionDiscoveryIncompleteReason::LimitExceeded,
        );

        let mut codex = base.clone();
        codex.retain_provider(&AgentProvider::Codex);
        assert_eq!(
            codex.incomplete,
            vec![
                SessionDiscoveryIncomplete {
                    source: SessionDiscoverySource::Casr,
                    reason: SessionDiscoveryIncompleteReason::Unavailable,
                },
                SessionDiscoveryIncomplete {
                    source: SessionDiscoverySource::Merged,
                    reason: SessionDiscoveryIncompleteReason::LimitExceeded,
                },
            ]
        );

        let mut antigravity = base;
        antigravity.retain_provider(&AgentProvider::Antigravity);
        assert_eq!(
            antigravity.incomplete,
            vec![
                SessionDiscoveryIncomplete {
                    source: SessionDiscoverySource::NativeAntigravity,
                    reason: SessionDiscoveryIncompleteReason::DirectoryUnreadable,
                },
                SessionDiscoveryIncomplete {
                    source: SessionDiscoverySource::Merged,
                    reason: SessionDiscoveryIncompleteReason::LimitExceeded,
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_home_overrides_a_different_configured_home_for_both_sources() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().expect("isolated casr-home fixture");
        let selected_home = fixture.path().join("selected-home");
        std::fs::create_dir(&selected_home).expect("create selected home");
        let casr = fixture.path().join("casr");
        let expected_home = selected_home.display().to_string();
        std::fs::write(
            &casr,
            format!(
                "#!/bin/sh\nif [ \"$HOME\" != '{}' ]; then exit 41; fi\nprintf '%s\\n' '[]'\n",
                expected_home.replace('\'', "'\\''")
            ),
        )
        .expect("write casr fixture");
        let mut permissions = std::fs::metadata(&casr)
            .expect("casr fixture metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&casr, permissions).expect("make casr fixture executable");

        let resumer = SessionResumer::new(SessionResumeConfig {
            casr_binary: casr.display().to_string(),
            home_dir: Some(fixture.path().join("different-configured-home")),
            ..Default::default()
        });
        let report = resumer
            .discover_sessions_in_home(&selected_home)
            .expect("CASR must observe the explicit selected home");
        assert!(report.entries.is_empty());
        assert!(report.is_complete());
    }

    #[cfg(unix)]
    #[test]
    fn default_discovery_uses_configured_home_for_both_sources() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().expect("isolated configured-home fixture");
        let configured_home = fixture.path().join("configured-home");
        std::fs::create_dir(&configured_home).expect("create configured home");
        let casr = fixture.path().join("casr");
        let expected_home = configured_home.display().to_string();
        std::fs::write(
            &casr,
            format!(
                "#!/bin/sh\nif [ \"$HOME\" != '{}' ]; then exit 41; fi\nprintf '%s\\n' '[]'\n",
                expected_home.replace('\'', "'\\''")
            ),
        )
        .expect("write casr fixture");
        let mut permissions = std::fs::metadata(&casr)
            .expect("casr fixture metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&casr, permissions).expect("make casr fixture executable");

        let resumer = SessionResumer::new(SessionResumeConfig {
            casr_binary: casr.display().to_string(),
            home_dir: Some(configured_home),
            ..Default::default()
        });
        let report = resumer
            .discover_sessions()
            .expect("both discovery sources must share configured home authority");
        assert!(report.entries.is_empty());
        assert!(report.is_complete());
    }

    #[test]
    fn is_casr_available_ignores_invalid_working_dir() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "rustc".into(),
            working_dir: Some(PathBuf::from("/definitely/nonexistent/casr-working-dir")),
            ..Default::default()
        });

        assert!(r.is_casr_available());
    }

    #[test]
    fn discover_sessions_invalid_working_dir_is_typed_partial_without_path() {
        let home = tempfile::tempdir().expect("isolated native discovery home");
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "rustc".into(),
            working_dir: Some(PathBuf::from("/definitely/nonexistent/casr-working-dir")),
            ..Default::default()
        });

        let report = r
            .discover_sessions_in_home(home.path())
            .expect("native inventory remains available when CASR cwd is invalid");
        assert!(report.incomplete.contains(&SessionDiscoveryIncomplete {
            source: SessionDiscoverySource::Casr,
            reason: SessionDiscoveryIncompleteReason::SubprocessFailed,
        }));
    }

    #[test]
    fn resumer_defaults_to_finite_canonical_capture_limits() {
        let resumer = SessionResumer::with_defaults();
        assert_eq!(resumer.stdout_limit, CASR_STDOUT_LIMIT_BYTES);
        assert_eq!(resumer.stderr_limit, CASR_STDERR_LIMIT_BYTES);
        assert!(resumer.stdout_limit > 0 && resumer.stdout_limit < usize::MAX);
        assert!(resumer.stderr_limit > 0 && resumer.stderr_limit < usize::MAX);
    }

    #[test]
    fn output_limit_overrides_reject_effectively_unbounded_capture() {
        let stdout_error = SessionResumer::with_defaults()
            .with_output_limits(MAX_CASR_STDOUT_LIMIT_BYTES + 1, MAX_CASR_STDERR_LIMIT_BYTES)
            .expect_err("stdout above the hard ceiling must be rejected");
        assert_eq!(
            stdout_error,
            SessionResumeError::InvalidOutputLimit {
                stream: CommandOutputStream::Stdout,
                requested: MAX_CASR_STDOUT_LIMIT_BYTES + 1,
                maximum: MAX_CASR_STDOUT_LIMIT_BYTES,
            }
        );

        let stderr_error = SessionResumer::with_defaults()
            .with_output_limits(MAX_CASR_STDOUT_LIMIT_BYTES, MAX_CASR_STDERR_LIMIT_BYTES + 1)
            .expect_err("stderr above the hard ceiling must be rejected");
        assert_eq!(
            stderr_error,
            SessionResumeError::InvalidOutputLimit {
                stream: CommandOutputStream::Stderr,
                requested: MAX_CASR_STDERR_LIMIT_BYTES + 1,
                maximum: MAX_CASR_STDERR_LIMIT_BYTES,
            }
        );
    }

    #[test]
    fn timeout_admission_rejects_zero_and_over_cap_before_spawn_setup() {
        for requested in [0, MAX_SESSION_RESUME_TIMEOUT_SECS + 1] {
            let resumer = SessionResumer::new(SessionResumeConfig {
                casr_binary: "/must-not-be-resolved-for-invalid-timeout".into(),
                working_dir: Some(PathBuf::from("/must-not-be-checked-for-invalid-timeout")),
                home_dir: Some(PathBuf::from("/must-not-be-exported-for-invalid-timeout")),
                timeout_secs: requested,
                dry_run: false,
            });
            assert_eq!(
                resumer.run_casr(&["--version"]),
                Err(SessionResumeError::InvalidTimeout {
                    requested,
                    maximum: MAX_SESSION_RESUME_TIMEOUT_SECS,
                })
            );
        }
        assert_eq!(
            validate_session_resume_timeout_secs(MAX_SESSION_RESUME_TIMEOUT_SECS),
            Ok(Duration::from_secs(MAX_SESSION_RESUME_TIMEOUT_SECS))
        );
    }

    #[test]
    fn merged_discovery_cap_fails_atomically_without_truncation() {
        let mut native = SessionDiscoveryResult {
            entries: (0..MAX_SESSION_DISCOVERY_ENTRIES)
                .map(|index| CasrListEntry {
                    session_id: format!("native-{index}"),
                    provider: Some("agy".to_string()),
                    title: None,
                    messages: 0,
                    workspace: None,
                    started_at: None,
                    path: None,
                    extra: HashMap::new(),
                })
                .collect(),
            incomplete: Vec::new(),
        };
        let before_len = native.entries.len();
        let error = merge_session_discovery_entries(
            &mut native,
            vec![CasrListEntry {
                session_id: "casr-extra".to_string(),
                provider: Some("codex".to_string()),
                title: None,
                messages: 0,
                workspace: None,
                started_at: None,
                path: None,
                extra: HashMap::new(),
            }],
        )
        .expect_err("the combined cap must fail rather than truncate");
        assert_eq!(
            error,
            SessionResumeError::DiscoveryLimitExceeded {
                source: SessionDiscoverySource::Merged,
                limit: MAX_SESSION_DISCOVERY_ENTRIES,
            }
        );
        assert_eq!(native.entries.len(), before_len);
    }

    #[cfg(unix)]
    #[test]
    fn run_casr_enforces_configured_timeout() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "sh".into(),
            timeout_secs: 1,
            ..Default::default()
        });

        let started = std::time::Instant::now();
        let err = r.run_casr(&["-c", "sleep 5"]).unwrap_err();

        assert_eq!(err, SessionResumeError::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "timeout should abort well before the child finishes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_casr_drains_large_stdout_without_deadlock() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "sh".into(),
            timeout_secs: 5,
            ..Default::default()
        });

        let output = r
            .run_casr(&["-c", "yes x | head -c 131072"])
            .expect("large stdout should not deadlock the CASR runner");

        assert_eq!(output.len(), 131072);
    }

    #[cfg(unix)]
    #[test]
    fn run_casr_accepts_exact_stdout_cap_and_rejects_one_byte_over() {
        let exact = SessionResumer::new(SessionResumeConfig {
            casr_binary: "sh".into(),
            timeout_secs: 5,
            ..Default::default()
        })
        .with_output_limits(3, 64)
        .expect("limits are below hard admission ceilings");
        assert_eq!(
            exact
                .run_casr(&["-c", "printf 'abc'"])
                .expect("exact stdout boundary must be accepted"),
            "abc"
        );

        let over = exact
            .clone()
            .with_output_limits(2, 64)
            .expect("limits are below hard admission ceilings");
        let error = over
            .run_casr(&["-c", "printf 'abc'; while :; do sleep 1; done"])
            .expect_err("first byte beyond stdout cap must fail closed");
        assert!(matches!(
            &error,
            SessionResumeError::CaptureLimitExceeded {
                stream: CommandOutputStream::Stdout,
                observed: 3,
                limit: 2,
            }
        ));
        assert!(!error.to_string().contains("abc"));
    }

    #[cfg(unix)]
    #[test]
    fn run_casr_accepts_exact_stderr_cap_and_rejects_one_byte_over() {
        let exact = SessionResumer::new(SessionResumeConfig {
            casr_binary: "sh".into(),
            timeout_secs: 5,
            ..Default::default()
        })
        .with_output_limits(64, 3)
        .expect("limits are below hard admission ceilings");
        assert_eq!(
            exact
                .run_casr(&["-c", "printf 'abc' >&2; printf 'ok'"])
                .expect("exact stderr boundary must be accepted"),
            "ok"
        );

        let over = exact
            .with_output_limits(64, 2)
            .expect("limits are below hard admission ceilings");
        let error = over
            .run_casr(&["-c", "printf 'abc' >&2; while :; do sleep 1; done"])
            .expect_err("first byte beyond stderr cap must fail closed");
        assert!(matches!(
            &error,
            SessionResumeError::CaptureLimitExceeded {
                stream: CommandOutputStream::Stderr,
                observed: 3,
                limit: 2,
            }
        ));
        assert!(!error.to_string().contains("abc"));
    }

    #[cfg(unix)]
    #[test]
    fn run_casr_high_volume_stdout_is_stopped_at_finite_cap() {
        const LIMIT: usize = 64 * 1024;
        let resumer = SessionResumer::new(SessionResumeConfig {
            casr_binary: "sh".into(),
            timeout_secs: 2,
            ..Default::default()
        })
        .with_output_limits(LIMIT, 64)
        .expect("limits are below hard admission ceilings");
        let error = resumer
            .run_casr(&["-c", "yes x"])
            .expect_err("unbounded producer must be stopped at the capture cap");
        assert!(matches!(
            &error,
            SessionResumeError::CaptureLimitExceeded {
                stream: CommandOutputStream::Stdout,
                observed,
                limit: LIMIT,
            } if *observed > LIMIT
        ));
    }

    #[cfg(unix)]
    #[test]
    fn run_casr_nonzero_stderr_is_not_retained_in_error() {
        let resumer = SessionResumer::new(SessionResumeConfig {
            casr_binary: "sh".into(),
            timeout_secs: 5,
            ..Default::default()
        });
        let error = resumer
            .run_casr(&["-c", "printf 'raw-child-output-canary' >&2; exit 19"])
            .expect_err("nonzero child must fail");
        assert!(matches!(
            &error,
            SessionResumeError::SubprocessFailed { code: Some(19), .. }
        ));
        assert!(!error.to_string().contains("raw-child-output-canary"));
    }

    #[cfg(unix)]
    #[test]
    fn run_casr_escaped_descendant_reports_incomplete_capture_finitely() {
        let resumer = SessionResumer::new(SessionResumeConfig {
            casr_binary: "sh".into(),
            timeout_secs: 5,
            ..Default::default()
        });
        let started = std::time::Instant::now();
        let error = resumer
            .run_casr(&["-c", "sleep 1 >&1 2>/dev/null &"])
            .expect_err("inherited output descriptor must fail closed");
        assert_eq!(
            error,
            SessionResumeError::CaptureIncomplete {
                stdout_open: true,
                stderr_open: false,
                drain_timeout_ms: 100,
            }
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn run_casr_cooperative_cancellation_is_finite() {
        let resumer = SessionResumer::new(SessionResumeConfig {
            casr_binary: "sh".into(),
            timeout_secs: 5,
            ..Default::default()
        });
        let cancellation = CommandCancellation::new();
        let cancel_from_thread = cancellation.clone();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            cancel_from_thread.cancel();
        });
        let started = std::time::Instant::now();
        let error = resumer
            .run_casr_with_options_and_cancellation(&["-c", "sleep 10"], true, Some(&cancellation))
            .expect_err("explicit cancellation must stop the subprocess");
        trigger.join().expect("cancellation trigger must finish");
        assert_eq!(error, SessionResumeError::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn production_casr_bridge_has_no_unbounded_reader_or_child_content_error_path() {
        let source = include_str!("session_resume.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix");
        let start = production
            .find("    fn run_casr(")
            .expect("casr subprocess implementation marker");
        let implementation = &production[start..];
        for forbidden in [
            "read_to_end",
            "std::thread::Builder",
            "std::thread::spawn",
            ".join()",
            "child.wait()",
            "from_utf8_lossy",
            "stderr = %",
            "binary = %",
            "args = ?",
        ] {
            assert!(
                !implementation.contains(forbidden),
                "production casr bridge must not contain {forbidden}"
            );
        }
    }

    #[test]
    fn public_cx_resume_reaches_the_cancellation_aware_supervisor() {
        std::hint::black_box(SessionResumer::resume_session_with_cx);
        let source = include_str!("session_resume.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix");
        let public_start = production
            .find("    pub async fn resume_session_with_cx(")
            .expect("public Cx-first resume API");
        let public_end = production[public_start..]
            .find("    /// List installed providers.")
            .map(|offset| public_start + offset)
            .expect("public Cx-first resume boundary");
        let public_resume = &production[public_start..public_end];
        assert!(public_resume.contains(".run_casr_with_cx(cx, &args)"));
        assert!(public_resume.contains("classify_resume_failure"));
        assert!(public_resume.contains("runtime_async::spawn_blocking(move"));
        assert!(!public_resume.contains("spawn_blocking_with_cx"));

        let bridge_start = production
            .find("    async fn run_casr_with_options_with_cx(")
            .expect("Cx-aware CASR bridge");
        let bridge = &production[bridge_start..];
        let timeout_validation = bridge
            .find("validate_session_resume_timeout_secs")
            .expect("timeout admission in async bridge");
        let command_build = bridge
            .find("self.build_casr_command")
            .expect("command construction in async bridge");
        assert!(timeout_validation < command_build);
        assert!(bridge.contains("output_blocking_with_cancellation"));
        assert!(bridge.contains("SessionCommandCancellationGuard::new"));
        assert!(bridge.contains("watcher_cx.checkpoint().is_err()"));
        assert!(bridge.contains("SESSION_COMMAND_CANCEL_REQUESTED"));
        assert!(bridge.contains("SESSION_COMMAND_WORKER_SETTLED"));
        let blocking_closure = bridge
            .find("let worker_result = crate::runtime_async::spawn_blocking(move ||")
            .expect("retained blocking worker");
        let settlement_cas = bridge[blocking_closure..]
            .find("SESSION_COMMAND_WORKER_SETTLED")
            .expect("worker settlement CAS inside closure")
            + blocking_closure;
        let worker_await = bridge[blocking_closure..]
            .find(".await;")
            .expect("blocking worker await")
            + blocking_closure;
        assert!(
            settlement_cas < worker_await,
            "command settlement must win before async scheduler handoff"
        );
        assert!(bridge.contains("task::try_spawn_with_cx"));
        assert!(bridge.contains("runtime_async::spawn_blocking(move"));
        assert!(!bridge.contains("spawn_blocking_with_cx(cx, move"));
    }

    #[test]
    fn settled_resume_envelope_cannot_report_success_when_casr_rejects_it() {
        let error = parse_casr_resume_output(r#"{"ok":false,"warnings":["denied"]}"#)
            .expect_err("a nested CASR failure must fail the outer operation");
        assert_eq!(error, SessionResumeError::ResumeRejected);

        let accepted = parse_casr_resume_output(r#"{"ok":true,"dry_run":true}"#)
            .expect("an explicit successful envelope should remain available");
        assert!(accepted.ok);
        assert!(accepted.dry_run);
    }

    #[test]
    fn mutating_resume_failures_are_indeterminate_but_dry_run_failures_are_not() {
        let mutating = SessionResumer::new(SessionResumeConfig::default());
        assert_eq!(
            mutating.classify_resume_failure(SessionResumeError::Cancelled),
            SessionResumeError::ResumeEffectIndeterminate {
                cause: ResumeEffectIndeterminateCause::Cancelled,
            }
        );
        assert_eq!(
            mutating.classify_resume_failure(SessionResumeError::ParseError { output_bytes: 9 }),
            SessionResumeError::ResumeEffectIndeterminate {
                cause: ResumeEffectIndeterminateCause::InvalidOutput,
            }
        );
        assert_eq!(
            mutating.classify_resume_failure(SessionResumeError::CasrNotFound),
            SessionResumeError::CasrNotFound,
            "pre-spawn unavailability remains safely retryable"
        );

        let dry_run = SessionResumer::new(SessionResumeConfig {
            dry_run: true,
            ..Default::default()
        });
        assert_eq!(
            dry_run.classify_resume_failure(SessionResumeError::Cancelled),
            SessionResumeError::Cancelled
        );
    }

    #[test]
    fn public_native_cx_discovery_never_routes_through_casr() {
        std::hint::black_box(discover_antigravity_conversations_from_home_with_cx);
        std::hint::black_box(discover_current_home_antigravity_conversations_with_cx);
        let source = include_str!("session_resume.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix");
        let start = production
            .find("struct NativeDiscoveryPermit;")
            .expect("runtime-owned native discovery boundary");
        let end = production[start..]
            .find("fn validate_discovery_entry(")
            .map(|offset| start + offset)
            .expect("native Cx API boundary");
        let native_cx_surface = &production[start..end];
        assert!(native_cx_surface.contains("runtime_async::spawn_blocking"));
        assert!(native_cx_surface.contains("task::try_spawn_with_cx"));
        assert!(native_cx_surface.contains("NativeDiscoveryObserverGuard"));
        assert!(native_cx_surface.contains("NativeDiscoveryTerminalReceipt"));
        assert!(native_cx_surface.contains("oneshot_recv_with_cx"));
        assert!(
            native_cx_surface
                .contains("discover_antigravity_conversations_from_home_with_checkpoint")
        );
        assert!(native_cx_surface.contains("scan_cx"));
        assert!(native_cx_surface.contains("checkpoint()"));
        assert!(!native_cx_surface.contains("spawn_blocking_with_cx"));
        assert!(!native_cx_surface.contains("run_casr"));
        assert!(!native_cx_surface.contains("build_casr_command"));
    }

    #[test]
    fn native_discovery_owner_is_cancel_independent_without_capability_escalation() {
        for (mask, expected) in crate::cx::capability_mask_test_cases() {
            let caller_cx = {
                let _caller_context =
                    crate::cx::Cx::set_current(Some(crate::cx::for_testing()));
                let _caller_capability_ceiling = crate::cx::Cx::push_restriction(mask);
                crate::cx::Cx::current().expect("restricted caller context")
            };
            let owner_cx = native_discovery_owner_cx(&caller_cx);
            assert_eq!(
                crate::cx::effective_capability_bits(&owner_cx),
                expected,
                "cleanup ownership must not regain a denied caller capability"
            );

            caller_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel caller after deriving native discovery owner"),
            );
            assert!(caller_cx.checkpoint().is_err());
            assert!(
                owner_cx.checkpoint().is_ok(),
                "cleanup ownership must survive caller cancellation"
            );
        }
    }

    #[test]
    fn native_discovery_owner_publishes_one_terminal_receipt() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("native discovery test runtime");
        runtime.block_on(async {
            let cx = crate::cx::for_request();
            let report = run_owned_native_discovery_with_cx(&cx, |_cancellation, _scan_cx| {
                Ok(SessionDiscoveryResult::default())
            })
            .await
            .expect("runtime-owned native discovery must return its receipt");
            assert!(report.entries.is_empty());
            wait_for_native_discovery_state(&cx, |snapshot| {
                snapshot.active_scans == before.active_scans
                    && snapshot.active_workers == before.active_workers
                    && snapshot.active_observers == before.active_observers
            })
            .await;
        });
        let after = native_discovery_runtime_metrics();
        assert_eq!(after.admitted_total, before.admitted_total + 1);
        assert_eq!(after.completed_total, before.completed_total + 1);
        assert_eq!(
            after.undelivered_receipt_total,
            before.undelivered_receipt_total
        );
    }

    #[test]
    fn dropping_unpolled_native_discovery_future_admits_no_work() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        let work_ran = Arc::new(AtomicBool::new(false));
        let work_ran_inner = Arc::clone(&work_ran);
        let cx = crate::cx::for_request();
        let request = run_owned_native_discovery_with_cx(
            &cx,
            move |_cancellation, _scan_cx| {
                work_ran_inner.store(true, Ordering::Release);
                Ok(SessionDiscoveryResult::default())
            },
        );

        drop(request);
        assert!(!work_ran.load(Ordering::Acquire));
        assert_eq!(native_discovery_runtime_metrics(), before);
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn native_discovery_worker_panic_is_quarantined_and_settles() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("native discovery panic runtime");
        runtime.block_on(async {
            let cx = crate::cx::for_request();
            let error = run_owned_native_discovery_with_cx(&cx, |_cancellation, _scan_cx| {
                panic!("synthetic native discovery worker panic");
            })
            .await
            .expect_err("worker panic must become a finite infrastructure failure");
            assert_eq!(error, SessionResumeError::AsyncInfrastructureFailure);
            wait_for_native_discovery_state(&cx, |snapshot| {
                snapshot.active_scans == before.active_scans
                    && snapshot.active_workers == before.active_workers
                    && snapshot.active_observers == before.active_observers
            })
            .await;
        });

        let after = native_discovery_runtime_metrics();
        assert_eq!(after.admitted_total, before.admitted_total + 1);
        assert_eq!(after.completed_total, before.completed_total + 1);
        assert_eq!(after.worker_failed_total, before.worker_failed_total + 1);
    }

    #[test]
    fn dropping_native_discovery_observer_cancels_but_owner_settles() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        let barrier = Arc::new(NativeDiscoveryTestBarrier::default());
        let worker_settled = Arc::new(AtomicBool::new(false));
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("native discovery test runtime");
        // Declared after the runtime so unwinding releases a blocked worker
        // before Runtime::drop begins its structured drain.
        let _release_on_drop = ReleaseNativeDiscoveryBarrierOnDrop(Arc::clone(&barrier));
        runtime.block_on(async {
            let request_cx = crate::cx::for_request();
            let task_cx = request_cx.clone();
            let worker_barrier = Arc::clone(&barrier);
            let worker_settled_inner = Arc::clone(&worker_settled);
            let request = crate::runtime_async::task::spawn_with_cx(
                &request_cx,
                move |_child_cx| async move {
                    run_owned_native_discovery_with_cx(
                        &task_cx,
                        move |cancellation, scan_cx| {
                            worker_barrier.wait_in_worker();
                            let result = cancellation.checkpoint(&scan_cx);
                            worker_settled_inner.store(true, Ordering::Release);
                            result?;
                            Ok(SessionDiscoveryResult::default())
                        },
                    )
                    .await
                },
            );

            wait_for_native_discovery_state(&request_cx, |snapshot| {
                barrier.started.load(Ordering::Acquire)
                    && snapshot.active_scans == before.active_scans + 1
                    && snapshot.active_workers == before.active_workers + 1
                    && snapshot.active_observers == before.active_observers + 1
            })
            .await;
            request.abort();
            let request_result = request.await;
            assert!(request_result.is_err(), "aborted observer must terminate");

            barrier.release();
            wait_for_native_discovery_state(&request_cx, |snapshot| {
                snapshot.active_scans == before.active_scans
                    && snapshot.active_workers == before.active_workers
                    && snapshot.active_observers == before.active_observers
            })
            .await;
        });

        let after = native_discovery_runtime_metrics();
        assert!(worker_settled.load(Ordering::Acquire));
        assert_eq!(after.admitted_total, before.admitted_total + 1);
        assert_eq!(after.completed_total, before.completed_total + 1);
        assert_eq!(
            after.cancel_requested_total,
            before.cancel_requested_total + 1
        );
        assert_eq!(
            after.dropped_observer_total,
            before.dropped_observer_total + 1
        );
        assert_eq!(
            after.undelivered_receipt_total,
            before.undelivered_receipt_total + 1
        );
    }

    #[test]
    fn repeated_observer_abandonment_settles_at_every_scan_checkpoint() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        let directory = tempfile::tempdir().expect("native checkpoint fixture");
        std::fs::write(
            directory
                .path()
                .join("123e4567-e89b-12d3-a456-426614174000.db"),
            b"SQLite format 3\0",
        )
        .expect("write native checkpoint fixture");

        let mut checkpoint_count = 0_usize;
        let report = discover_antigravity_conversations_in_dir_with_checkpoint(
            directory.path(),
            || {
                checkpoint_count = checkpoint_count.saturating_add(1);
                Ok(())
            },
        )
        .expect("enumerate native checkpoint fixture");
        assert_eq!(report.entries.len(), 1);
        assert!(checkpoint_count > 0);

        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("native checkpoint runtime");
        for target in 1..=checkpoint_count {
            let gate = Arc::new(NativeDiscoveryCheckpointGate::new(target));
            let _release_on_drop =
                ReleaseNativeDiscoveryCheckpointOnDrop(Arc::clone(&gate));
            runtime.block_on(async {
                let request_cx = crate::cx::for_request();
                let task_cx = request_cx.clone();
                let worker_gate = Arc::clone(&gate);
                let fixture_path = directory.path().to_path_buf();
                let request = crate::runtime_async::task::spawn_with_cx(
                    &request_cx,
                    move |_child_cx| async move {
                        run_owned_native_discovery_with_cx(
                            &task_cx,
                            move |cancellation, scan_cx| {
                                discover_antigravity_conversations_in_dir_with_checkpoint(
                                    &fixture_path,
                                    || {
                                        worker_gate.checkpoint_in_worker();
                                        cancellation.checkpoint(&scan_cx)
                                    },
                                )
                            },
                        )
                        .await
                    },
                );

                wait_for_native_discovery_state(&request_cx, |snapshot| {
                    gate.reached.load(Ordering::Acquire) >= target
                        && snapshot.active_scans == before.active_scans + 1
                        && snapshot.active_workers == before.active_workers + 1
                        && snapshot.active_observers == before.active_observers + 1
                })
                .await;
                request.abort();
                assert!(
                    request.await.is_err(),
                    "checkpoint {target} observer must acknowledge abort"
                );
                gate.release();
                wait_for_native_discovery_state(&request_cx, |snapshot| {
                    snapshot.active_scans == before.active_scans
                        && snapshot.active_workers == before.active_workers
                        && snapshot.active_observers == before.active_observers
                })
                .await;
            });
        }

        let checkpoint_count_u64 = u64::try_from(checkpoint_count).expect("finite checkpoints");
        let after = native_discovery_runtime_metrics();
        assert_eq!(
            after.admitted_total,
            before.admitted_total + checkpoint_count_u64
        );
        assert_eq!(
            after.completed_total,
            before.completed_total + checkpoint_count_u64
        );
        assert_eq!(
            after.cancel_requested_total,
            before.cancel_requested_total + checkpoint_count_u64
        );
        assert_eq!(
            after.dropped_observer_total,
            before.dropped_observer_total + checkpoint_count_u64
        );
        assert_eq!(
            after.undelivered_receipt_total,
            before.undelivered_receipt_total + checkpoint_count_u64
        );
    }

    #[test]
    fn cancelling_native_discovery_context_cancels_but_owner_settles() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        let barrier = Arc::new(NativeDiscoveryTestBarrier::default());
        let worker_settled = Arc::new(AtomicBool::new(false));
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("native discovery test runtime");
        // Declared after the runtime so unwinding releases a blocked worker
        // before Runtime::drop begins its structured drain.
        let _release_on_drop = ReleaseNativeDiscoveryBarrierOnDrop(Arc::clone(&barrier));
        runtime.block_on(async {
            let request_cx = crate::cx::for_request();
            let task_cx = request_cx.clone();
            let settlement_cx = crate::cx::for_request();
            let worker_barrier = Arc::clone(&barrier);
            let worker_settled_inner = Arc::clone(&worker_settled);
            let request = crate::runtime_async::task::spawn_with_cx(
                &request_cx,
                move |_child_cx| async move {
                    run_owned_native_discovery_with_cx(
                        &task_cx,
                        move |cancellation, scan_cx| {
                            worker_barrier.wait_in_worker();
                            let result = cancellation.checkpoint(&scan_cx);
                            worker_settled_inner.store(true, Ordering::Release);
                            result?;
                            Ok(SessionDiscoveryResult::default())
                        },
                    )
                    .await
                },
            );

            wait_for_native_discovery_state(&settlement_cx, |snapshot| {
                barrier.started.load(Ordering::Acquire)
                    && snapshot.active_scans == before.active_scans + 1
                    && snapshot.active_workers == before.active_workers + 1
                    && snapshot.active_observers == before.active_observers + 1
            })
            .await;
            request_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel native discovery caller context"),
            );
            assert_eq!(
                request
                    .await
                    .expect("observer task must return its typed result"),
                Err(SessionResumeError::Cancelled)
            );

            barrier.release();
            wait_for_native_discovery_state(&settlement_cx, |snapshot| {
                snapshot.active_scans == before.active_scans
                    && snapshot.active_workers == before.active_workers
                    && snapshot.active_observers == before.active_observers
            })
            .await;
        });

        let after = native_discovery_runtime_metrics();
        assert!(worker_settled.load(Ordering::Acquire));
        assert_eq!(after.admitted_total, before.admitted_total + 1);
        assert_eq!(after.completed_total, before.completed_total + 1);
        assert_eq!(
            after.cancel_requested_total,
            before.cancel_requested_total + 1
        );
        assert_eq!(
            after.dropped_observer_total, before.dropped_observer_total,
            "explicit context cancellation is not an abandoned observer"
        );
        assert_eq!(
            after.undelivered_receipt_total,
            before.undelivered_receipt_total + 1
        );
    }

    #[test]
    fn native_discovery_deadline_remains_timeout_and_owner_settles() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        let barrier = Arc::new(NativeDiscoveryTestBarrier::default());
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("native discovery deadline runtime");
        let _release_on_drop = ReleaseNativeDiscoveryBarrierOnDrop(Arc::clone(&barrier));
        runtime.block_on(async {
            let request_cx = crate::cx::for_request();
            let task_cx = request_cx.clone();
            let settlement_cx = crate::cx::for_request();
            let worker_barrier = Arc::clone(&barrier);
            let request = crate::runtime_async::task::spawn_with_cx(
                &request_cx,
                move |_child_cx| async move {
                    run_owned_native_discovery_with_cx(
                        &task_cx,
                        move |cancellation, scan_cx| {
                            worker_barrier.wait_in_worker();
                            cancellation.checkpoint(&scan_cx)?;
                            Ok(SessionDiscoveryResult::default())
                        },
                    )
                    .await
                },
            );

            wait_for_native_discovery_state(&settlement_cx, |snapshot| {
                barrier.started.load(Ordering::Acquire)
                    && snapshot.active_scans == before.active_scans + 1
                    && snapshot.active_workers == before.active_workers + 1
                    && snapshot.active_observers == before.active_observers + 1
            })
            .await;
            request_cx.cancel_with(
                crate::outcome::CancelKind::Deadline,
                Some("expire synthetic native discovery deadline"),
            );
            assert_eq!(
                request
                    .await
                    .expect("deadline observer task must return its typed result"),
                Err(SessionResumeError::Timeout)
            );

            barrier.release();
            wait_for_native_discovery_state(&settlement_cx, |snapshot| {
                snapshot.active_scans == before.active_scans
                    && snapshot.active_workers == before.active_workers
                    && snapshot.active_observers == before.active_observers
            })
            .await;
        });

        let after = native_discovery_runtime_metrics();
        assert_eq!(after.admitted_total, before.admitted_total + 1);
        assert_eq!(after.completed_total, before.completed_total + 1);
        assert_eq!(
            after.cancel_requested_total,
            before.cancel_requested_total + 1
        );
        assert_eq!(
            after.dropped_observer_total, before.dropped_observer_total,
            "a deadline is not an abandoned observer"
        );
        assert_eq!(
            after.undelivered_receipt_total,
            before.undelivered_receipt_total + 1
        );
    }

    #[test]
    fn runtime_shutdown_cancels_and_drains_native_discovery_owner() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        let barrier = Arc::new(NativeDiscoveryTestBarrier::default());
        let worker_barrier = Arc::clone(&barrier);
        let (cancellation_tx, cancellation_rx) = std::sync::mpsc::sync_channel(1);
        let runtime = crate::runtime_async::RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .build()
            .expect("native discovery shutdown runtime");
        runtime.spawn_detached(async move {
            let request_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let _ = run_owned_native_discovery_with_cx(
                &request_cx,
                move |cancellation, scan_cx| {
                    cancellation_tx
                        .send(cancellation.clone())
                        .expect("publish native shutdown cancellation token");
                    worker_barrier.wait_in_worker();
                    cancellation.checkpoint(&scan_cx)?;
                    Ok(SessionDiscoveryResult::default())
                },
            )
            .await;
        });

        let cancellation = cancellation_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("native shutdown worker must start");
        wait_for_native_discovery_state_blocking(|snapshot| {
            barrier.started.load(Ordering::Acquire)
                && snapshot.active_scans == before.active_scans + 1
                && snapshot.active_workers == before.active_workers + 1
                && snapshot.active_observers == before.active_observers + 1
        });

        let release_barrier = Arc::clone(&barrier);
        let shutdown_observed = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while !cancellation.runtime_shutdown.is_shutdown_requested()
                && started.elapsed() < Duration::from_secs(5)
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            let observed = cancellation.runtime_shutdown.is_shutdown_requested();
            release_barrier.release();
            observed
        });

        drop(runtime);
        assert!(
            shutdown_observed
                .join()
                .expect("native shutdown observer thread must not panic"),
            "runtime drop must publish shutdown before draining native work"
        );
        wait_for_native_discovery_state_blocking(|snapshot| {
            snapshot.active_scans == before.active_scans
                && snapshot.active_workers == before.active_workers
                && snapshot.active_observers == before.active_observers
        });

        let after = native_discovery_runtime_metrics();
        assert_eq!(after.admitted_total, before.admitted_total + 1);
        assert_eq!(after.completed_total, before.completed_total + 1);
        assert_eq!(
            after.cancel_requested_total,
            before.cancel_requested_total + 1
        );
        assert_eq!(
            after.undelivered_receipt_total,
            before.undelivered_receipt_total + 1
        );
    }

    #[test]
    fn native_discovery_runtime_rejection_releases_permit_without_work() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        assert_eq!(before.active_scans, 0, "lifecycle tests must start quiescent");
        let work_ran = Arc::new(AtomicBool::new(false));
        let work_ran_inner = Arc::clone(&work_ran);

        // A fresh OS thread has no installed runtime handle. Polling once is
        // sufficient because rejection occurs before the first suspension or
        // filesystem effect.
        std::thread::spawn(move || {
            use std::future::Future as _;

            let cx = crate::cx::for_request();
            let mut request = Box::pin(run_owned_native_discovery_with_cx(
                &cx,
                move |_cancellation, _scan_cx| {
                    work_ran_inner.store(true, Ordering::Release);
                    Ok(SessionDiscoveryResult::default())
                },
            ));
            let waker = futures::task::noop_waker();
            let mut poll_cx = std::task::Context::from_waker(&waker);
            let error = match request.as_mut().poll(&mut poll_cx) {
                std::task::Poll::Ready(Err(error)) => error,
                other => panic!("missing runtime must reject before suspending: {other:?}"),
            };
            assert_eq!(
                error,
                SessionResumeError::DiscoveryAdmissionRejected {
                    reason:
                        SessionDiscoveryAdmissionRejection::RuntimeUnavailableOrShuttingDown,
                }
            );
        })
        .join()
        .expect("runtime-rejection probe thread must not panic");

        let after = native_discovery_runtime_metrics();
        assert!(!work_ran.load(Ordering::Acquire));
        assert_eq!(after.active_scans, before.active_scans);
        assert_eq!(after.active_workers, before.active_workers);
        assert_eq!(after.active_observers, before.active_observers);
        assert_eq!(
            after.admitted_total, before.admitted_total,
            "missing application-runtime authority must reject before subsystem admission"
        );
        assert_eq!(
            after.runtime_rejected_total,
            before.runtime_rejected_total + 1
        );
    }

    #[test]
    fn native_discovery_saturation_rejects_without_running_work() {
        let _test_lock = native_discovery_lifecycle_test_lock();
        let before = native_discovery_runtime_metrics();
        assert_eq!(before.active_scans, 0, "lifecycle tests must start quiescent");
        let barrier = Arc::new(NativeDiscoveryTestBarrier::default());
        let started = Arc::new(AtomicUsize::new(0));
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("native discovery test runtime");
        // Declared after the runtime so unwinding releases every worker before
        // Runtime::drop begins its structured drain.
        let _release_on_drop = ReleaseNativeDiscoveryBarrierOnDrop(Arc::clone(&barrier));
        runtime.block_on(async {
            let wait_cx = crate::cx::for_request();
            let mut observers = Vec::with_capacity(MAX_CONCURRENT_NATIVE_DISCOVERY_SCANS);
            for _ in 0..MAX_CONCURRENT_NATIVE_DISCOVERY_SCANS {
                let request_cx = crate::cx::for_request();
                let task_cx = request_cx.clone();
                let worker_barrier = Arc::clone(&barrier);
                let worker_started = Arc::clone(&started);
                observers.push(crate::runtime_async::task::spawn_with_cx(
                    &request_cx,
                    move |_child_cx| async move {
                        run_owned_native_discovery_with_cx(
                            &task_cx,
                            move |cancellation, scan_cx| {
                                worker_started.fetch_add(1, Ordering::AcqRel);
                                worker_barrier.wait_in_worker();
                                cancellation.checkpoint(&scan_cx)?;
                                Ok(SessionDiscoveryResult::default())
                            },
                        )
                        .await
                    },
                ));
            }
            wait_for_native_discovery_state(&wait_cx, |snapshot| {
                started.load(Ordering::Acquire) == MAX_CONCURRENT_NATIVE_DISCOVERY_SCANS
                    && snapshot.active_scans == MAX_CONCURRENT_NATIVE_DISCOVERY_SCANS
            })
            .await;

            let rejected_work_ran = Arc::new(AtomicBool::new(false));
            let rejected_work_ran_inner = Arc::clone(&rejected_work_ran);
            let error = run_owned_native_discovery_with_cx(
                &wait_cx,
                move |_cancellation, _scan_cx| {
                    rejected_work_ran_inner.store(true, Ordering::Release);
                    Ok(SessionDiscoveryResult::default())
                },
            )
            .await
            .expect_err("the fifth concurrent scan must fail before owner admission");
            assert_eq!(
                error,
                SessionResumeError::DiscoveryAdmissionRejected {
                    reason: SessionDiscoveryAdmissionRejection::SubsystemSaturated,
                }
            );
            assert!(!rejected_work_ran.load(Ordering::Acquire));

            for observer in &observers {
                observer.abort();
            }
            for observer in observers {
                assert!(observer.await.is_err());
            }
            barrier.release();
            wait_for_native_discovery_state(&wait_cx, |snapshot| {
                snapshot.active_scans == 0
                    && snapshot.active_workers == 0
                    && snapshot.active_observers == 0
            })
            .await;
        });

        let after = native_discovery_runtime_metrics();
        assert_eq!(
            after.saturated_total,
            before.saturated_total + 1,
            "one rejected admission must remain observable"
        );
        assert_eq!(
            after.admitted_total,
            before.admitted_total + MAX_CONCURRENT_NATIVE_DISCOVERY_SCANS as u64
        );
    }

    #[test]
    fn native_scan_observes_cancellation_after_empty_enumeration() {
        let directory = tempfile::tempdir().expect("empty native scan directory");
        let mut checkpoints = 0_usize;
        let error =
            discover_antigravity_conversations_in_dir_with_checkpoint(directory.path(), || {
                checkpoints = checkpoints.saturating_add(1);
                if checkpoints == 2 {
                    Err(SessionResumeError::Cancelled)
                } else {
                    Ok(())
                }
            })
            .expect_err("final checkpoint must observe empty-scan cancellation");
        assert_eq!(error, SessionResumeError::Cancelled);
    }

    #[test]
    fn native_scan_observes_cancellation_after_last_entry() {
        let directory = tempfile::tempdir().expect("native scan directory");
        std::fs::write(
            directory
                .path()
                .join("123e4567-e89b-12d3-a456-426614174000.db"),
            b"SQLite format 3\0fixture",
        )
        .expect("write native SQLite fixture");
        let mut checkpoints = 0_usize;
        let error =
            discover_antigravity_conversations_in_dir_with_checkpoint(directory.path(), || {
                checkpoints = checkpoints.saturating_add(1);
                if checkpoints == 3 {
                    Err(SessionResumeError::Cancelled)
                } else {
                    Ok(())
                }
            })
            .expect_err("final checkpoint must observe last-entry cancellation");
        assert_eq!(error, SessionResumeError::Cancelled);
    }
}
