//! Session resume orchestrator — bridges FrankenTerm ↔ `casr` CLI.
//!
//! Wraps `cross_agent_session_resumer` subprocess calls for discovering,
//! resuming, and exporting agent sessions across providers (Claude Code,
//! Codex, Gemini, etc.).
//!
//! Feature-gated behind `session-resume`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::casr_types::{
    CanonicalMessage, CanonicalSession, CasrListEntry, CasrProviderStatus, CasrResumeOutput,
};
use crate::runtime_async::process::{
    Command, CommandCancellation, CommandCancelled, CommandCleanupTrigger,
    CommandOutputCaptureIncomplete, CommandOutputLimitExceeded,
    CommandOutputStream, CommandProcessCleanupIncomplete, CommandTimedOut,
    DEFAULT_COMMAND_STDERR_LIMIT_BYTES, DEFAULT_COMMAND_STDOUT_LIMIT_BYTES,
    decode_captured_bytes_lossy,
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
/// Maximum number of sessions admitted from all discovery sources combined.
pub const MAX_SESSION_DISCOVERY_ENTRIES: usize = 10_000;
/// Maximum bytes admitted for a session identifier crossing into argv or a
/// public discovery result.
pub const MAX_SESSION_RESUME_ID_BYTES: usize = 256;
/// Maximum bytes admitted for a provider slug crossing into argv.
pub const MAX_SESSION_RESUME_PROVIDER_BYTES: usize = 64;
const SESSION_RESUME_CX_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Finite identity for a session-discovery source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDiscoverySource {
    Casr,
    NativeAntigravity,
    Merged,
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
    InvalidOutput,
    OutputCaptureIncomplete,
    LimitExceeded,
    DirectoryUnreadable,
    DirectoryEntryUnreadable,
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
    let trimmed_id = session_id.trim();
    if !is_valid_antigravity_conversation_id(trimmed_id) {
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
        session_id: trimmed_id.to_string(),
        binary: ANTIGRAVITY_BINARY.to_string(),
        argv: vec![
            ANTIGRAVITY_BINARY.to_string(),
            "--conversation".to_string(),
            trimmed_id.to_string(),
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

/// Error type for session resume operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionResumeError {
    /// The casr binary was not found or not executable.
    CasrNotFound,
    /// The subprocess exited with a non-zero code.
    SubprocessFailed { code: Option<i32> },
    /// Failed to parse JSON output from casr.
    ParseError { output_bytes: usize },
    /// The requested session was not found.
    SessionNotFound { identifier_bytes: usize },
    /// Provider is not installed.
    ProviderNotInstalled,
    /// Native provider binary was not found.
    NativeProviderNotFound,
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
    /// A caller-requested capture limit exceeded the bridge's hard admission
    /// ceiling.
    InvalidOutputLimit {
        stream: CommandOutputStream,
        requested: usize,
        maximum: usize,
    },
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
            Self::SessionNotFound { identifier_bytes } => {
                write!(f, "session not found (identifier_bytes={identifier_bytes})")
            }
            Self::ProviderNotInstalled => f.write_str("provider not installed"),
            Self::NativeProviderNotFound => {
                f.write_str("native provider binary unavailable")
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
            Self::InvalidSessionIdentifier { input_bytes } => write!(
                f,
                "invalid session identifier (input_bytes={input_bytes})"
            ),
            Self::InvalidProviderSlug { input_bytes } => {
                write!(f, "invalid provider slug (input_bytes={input_bytes})")
            }
            Self::WorkingDirectoryUnavailable => {
                f.write_str("session-resume working directory unavailable")
            }
            Self::InvalidOutputLimit {
                stream,
                requested,
                maximum,
            } => write!(
                f,
                "invalid casr {stream} capture limit ({requested} > {maximum})"
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

fn validate_session_identifier(session_id: &str) -> Result<(), SessionResumeError> {
    if session_id.is_empty()
        || session_id.len() > MAX_SESSION_RESUME_ID_BYTES
        || session_id.chars().any(char::is_control)
    {
        return Err(SessionResumeError::InvalidSessionIdentifier {
            input_bytes: session_id.len(),
        });
    }
    Ok(())
}

fn validate_provider_slug(provider_slug: &str) -> Result<(), SessionResumeError> {
    if provider_slug.is_empty()
        || provider_slug.len() > MAX_SESSION_RESUME_PROVIDER_BYTES
        || !provider_slug.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(SessionResumeError::InvalidProviderSlug {
            input_bytes: provider_slug.len(),
        });
    }
    Ok(())
}

fn map_spawn_blocking_error(
    error: &crate::runtime_async::SpawnBlockingWithCxError,
) -> SessionResumeError {
    if error.is_cancelled() {
        SessionResumeError::Cancelled
    } else {
        SessionResumeError::AsyncInfrastructureFailure
    }
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
    #[must_use]
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
        self.discover_sessions_with_native_antigravity(
            discover_current_home_antigravity_conversations()?,
        )
    }

    /// Discover sessions using an explicit home directory for native provider scans.
    ///
    /// This is useful for tests and automation that need deterministic provider
    /// fixtures without mutating process-wide `HOME`.
    pub fn discover_sessions_in_home(
        &self,
        home_dir: &Path,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        self.discover_sessions_with_native_antigravity(
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
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);
        let native = discover_antigravity_conversations_with_cx(cx, home).await?;
        self.discover_sessions_with_native_antigravity_with_cx(cx, native)
            .await
    }

    /// Cx-first discovery rooted at an explicit home directory.
    pub async fn discover_sessions_in_home_with_cx(
        &self,
        cx: &crate::cx::Cx,
        home_dir: &Path,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        let native =
            discover_antigravity_conversations_with_cx(cx, Some(home_dir.to_path_buf())).await?;
        self.discover_sessions_with_native_antigravity_with_cx(cx, native)
            .await
    }

    fn discover_sessions_with_native_antigravity(
        &self,
        mut native: SessionDiscoveryResult,
    ) -> Result<SessionDiscoveryResult, SessionResumeError> {
        info!(session_resume = true, "discovering sessions");

        match self.run_casr(&["list", "--json"]) {
            Ok(output) => {
                match parse_casr_discovery_entries(&output) {
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
        let mut report = self.discover_sessions()?;
        let slug = provider.slug();
        validate_provider_slug(slug)?;
        report
            .entries
            .retain(|entry| entry.provider.as_deref() == Some(slug));
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
        report
            .entries
            .retain(|entry| entry.provider.as_deref() == Some(slug));
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

        let output = self.run_casr(&args)?;
        let output_bytes = output.len();
        let result: CasrResumeOutput = serde_json::from_str(&output)
            .map_err(|_| SessionResumeError::ParseError { output_bytes })?;

        if !result.ok {
            warn!(session_resume = true, "resume reported failure");
        }

        Ok(result)
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

        let output = self.run_casr_with_cx(cx, &args).await?;
        let output_bytes = output.len();
        let result = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            serde_json::from_str::<CasrResumeOutput>(&output)
                .map_err(|_| SessionResumeError::ParseError { output_bytes })
        })
        .await
        .map_err(|error| map_spawn_blocking_error(&error))??;

        if !result.ok {
            warn!(session_resume = true, "resume reported failure");
        }
        Ok(result)
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
        Ok(providers.iter().any(|candidate| {
            candidate.slug == slug && candidate.installed
        }))
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
        let mut cmd = self.build_casr_command(args, apply_working_dir)?;

        let timeout = Duration::from_secs(self.config.timeout_secs);
        let output = match cancellation {
            Some(cancellation) => cmd
                .output_blocking_with_cancellation(timeout, cancellation)
                .map_err(|error| self.map_command_error(&error))?,
            None => cmd
                .output_blocking(timeout)
                .map_err(|error| self.map_command_error(&error))?,
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
        cx.checkpoint().map_err(|_| SessionResumeError::Cancelled)?;
        let mut cmd = self.build_casr_command(args, apply_working_dir)?;
        let timeout = Duration::from_secs(self.config.timeout_secs);
        let cancellation = CommandCancellation::new();
        let worker_cancellation = cancellation.clone();
        let watcher_cancellation = cancellation.clone();
        let watcher_done = Arc::new(AtomicBool::new(false));
        let watcher_done_inner = Arc::clone(&watcher_done);
        let watcher_failed = Arc::new(AtomicBool::new(false));
        let watcher_failed_inner = Arc::clone(&watcher_failed);
        let watcher_cx = cx.clone();
        let watcher_handle = crate::runtime_async::task::spawn_with_cx(
            cx,
            move |_child_cx| async move {
                while !watcher_done_inner.load(Ordering::SeqCst) {
                    if watcher_cx.checkpoint().is_err() {
                        watcher_cancellation.cancel();
                        return;
                    }
                    if crate::runtime_async::sleep_with_cx(
                        &watcher_cx,
                        SESSION_RESUME_CX_POLL_INTERVAL,
                    )
                    .await
                    .is_err()
                    {
                        if watcher_cx.checkpoint().is_ok() {
                            watcher_failed_inner.store(true, Ordering::SeqCst);
                        }
                        watcher_cancellation.cancel();
                        return;
                    }
                }
            },
        );
        let mut cancellation_guard =
            SessionCommandCancellationGuard::new(cancellation, Arc::clone(&watcher_done));

        let worker_result = crate::runtime_async::spawn_blocking(move || {
            cmd.output_blocking_with_cancellation(timeout, &worker_cancellation)
        })
        .await;
        watcher_done.store(true, Ordering::SeqCst);
        let watcher_result = watcher_handle.await;

        if worker_result.is_err()
            || watcher_result.is_err()
            || watcher_failed.load(Ordering::SeqCst)
        {
            return Err(SessionResumeError::AsyncInfrastructureFailure);
        }
        let output = worker_result
            .map_err(|_| SessionResumeError::AsyncInfrastructureFailure)?
            .map_err(|error| self.map_command_error(&error))?;
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

        if apply_working_dir
            && let Some(ref dir) = self.config.working_dir
        {
            if !dir.is_dir() {
                return Err(SessionResumeError::WorkingDirectoryUnavailable);
            }
            cmd.current_dir(dir);
        }
        Ok(cmd)
    }

    fn map_command_error(&self, err: &std::io::Error) -> SessionResumeError {
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
            let (source, reason) = discovery_error_incomplete_evidence(&error);
            let mut report = SessionDiscoveryResult::default();
            report.mark_incomplete(source, reason);
            report
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

/// Discover native Antigravity conversations under a testable home directory.
///
/// The Antigravity CLI stores one SQLite database per conversation at
/// `~/.gemini/antigravity-cli/conversations/<uuid>.db`; that filename stem is
/// the id accepted by `agy --conversation <uuid>`.
pub fn discover_antigravity_conversations_from_home(
    home_dir: &Path,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    discover_antigravity_conversations_in_dir(&antigravity_conversations_dir(home_dir))
}

/// Discover native Antigravity conversations under an explicit conversations dir.
pub fn discover_antigravity_conversations_in_dir(
    conversations_dir: &Path,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    let dir_entries = match fs::read_dir(conversations_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionDiscoveryResult::default());
        }
        Err(err) => {
            warn!(
                session_resume = true,
                provider = "agy",
                error_kind = ?err.kind(),
                "failed to read Antigravity conversations directory"
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
    for (entry_index, dir_entry) in dir_entries.enumerate() {
        if entry_index >= MAX_SESSION_DISCOVERY_ENTRIES {
            return Err(SessionResumeError::DiscoveryLimitExceeded {
                source: SessionDiscoverySource::NativeAntigravity,
                limit: MAX_SESSION_DISCOVERY_ENTRIES,
            });
        }
        let dir_entry = match dir_entry {
            Ok(entry) => entry,
            Err(err) => {
                warn!(
                    session_resume = true,
                    provider = "agy",
                    error_kind = ?err.kind(),
                    "failed to read Antigravity conversation directory entry"
                );
                report.mark_incomplete(
                    SessionDiscoverySource::NativeAntigravity,
                    SessionDiscoveryIncompleteReason::DirectoryEntryUnreadable,
                );
                continue;
            }
        };
        let path = dir_entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("db") {
            continue;
        }

        let Some(session_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.trim().is_empty())
            .map(str::to_string)
        else {
            continue;
        };

        if !is_valid_antigravity_conversation_id(&session_id) {
            warn!(
                session_resume = true,
                provider = "agy",
                candidate_bytes = session_id.len(),
                "ignored Antigravity conversation database with invalid UUID filename stem"
            );
            continue;
        }

        let started_at = dir_entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(system_time_to_epoch_millis);
        let resume_plan = antigravity_native_resume_plan(&session_id)?;
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
            path: Some(path.display().to_string()),
            extra,
        });
    }

    report.entries.sort_by(|left, right| {
        left.session_id
            .cmp(&right.session_id)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(report)
}

/// Discover Antigravity conversations under the process HOME, if available.
pub fn discover_current_home_antigravity_conversations(
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    let Some(home) = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(SessionDiscoveryResult::default());
    };
    discover_antigravity_conversations_from_home(&home)
}

async fn discover_antigravity_conversations_with_cx(
    cx: &crate::cx::Cx,
    home_dir: Option<PathBuf>,
) -> Result<SessionDiscoveryResult, SessionResumeError> {
    let Some(home_dir) = home_dir else {
        cx.checkpoint().map_err(|_| SessionResumeError::Cancelled)?;
        return Ok(SessionDiscoveryResult::default());
    };
    crate::runtime_async::spawn_blocking_with_cx(cx, move || {
        discover_antigravity_conversations_from_home(&home_dir)
    })
    .await
    .map_err(|error| map_spawn_blocking_error(&error))?
}

fn validate_discovery_entry(entry: &CasrListEntry) -> Result<(), SessionResumeError> {
    validate_session_identifier(&entry.session_id)?;
    if let Some(provider) = entry.provider.as_deref() {
        validate_provider_slug(provider)?;
    }
    Ok(())
}

fn parse_casr_discovery_entries(
    output: &str,
) -> Result<Vec<CasrListEntry>, SessionResumeError> {
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
    for entry in casr_entries.into_iter().chain(native.entries.iter().cloned()) {
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
        SessionResumeError::SubprocessFailed { .. }
        | SessionResumeError::WorkingDirectoryUnavailable => {
            Some(SessionDiscoveryIncompleteReason::SubprocessFailed)
        }
        SessionResumeError::ParseError { .. }
        | SessionResumeError::InvalidSessionIdentifier { .. }
        | SessionResumeError::InvalidProviderSlug { .. }
        | SessionResumeError::CaptureLimitExceeded { .. } => {
            Some(SessionDiscoveryIncompleteReason::InvalidOutput)
        }
        SessionResumeError::CaptureIncomplete { .. } => {
            Some(SessionDiscoveryIncompleteReason::OutputCaptureIncomplete)
        }
        SessionResumeError::DiscoveryLimitExceeded { .. }
        | SessionResumeError::InvalidOutputLimit { .. }
        | SessionResumeError::AsyncInfrastructureFailure
        | SessionResumeError::Cancelled
        | SessionResumeError::CleanupIncomplete { .. }
        | SessionResumeError::SessionNotFound { .. }
        | SessionResumeError::ProviderNotInstalled
        | SessionResumeError::NativeProviderNotFound
        | SessionResumeError::InvalidNativeSessionId { .. }
        | SessionResumeError::NonPinnedNativeModel { .. } => None,
    }
}

fn discovery_error_incomplete_evidence(
    error: &SessionResumeError,
) -> (SessionDiscoverySource, SessionDiscoveryIncompleteReason) {
    match error {
        SessionResumeError::DiscoveryLimitExceeded { source, .. } => {
            (*source, SessionDiscoveryIncompleteReason::LimitExceeded)
        }
        _ => (
            SessionDiscoverySource::Merged,
            casr_discovery_incomplete_reason(error)
                .unwrap_or(SessionDiscoveryIncompleteReason::SubprocessFailed),
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
        entry.provider.as_deref().unwrap_or("?"),
        32,
        MAX_SESSION_RESUME_PROVIDER_BYTES,
    );
    let session_id = crate::output::truncate_bounded(
        &entry.session_id,
        80,
        MAX_SESSION_RESUME_ID_BYTES,
    );
    let title = crate::output::truncate_bounded(
        entry.title.as_deref().unwrap_or("(untitled)"),
        60,
        256,
    );
    let msgs = entry.messages;
    format!(
        "[{}] {} ({} msgs) — {}",
        provider, session_id, msgs, title
    )
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::casr_types::MessageRole;
    use serde_json::json;
    use std::collections::HashMap;

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
        let plan =
            antigravity_native_resume_plan("123e4567-e89b-12d3-a456-426614174000").unwrap();
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

        let plan =
            antigravity_native_resume_plan("123e4567-e89b-12d3-a456-426614174000").unwrap();
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
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = SessionResumeConfig {
            casr_binary: "/usr/bin/casr".into(),
            working_dir: Some(PathBuf::from("/project")),
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
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let report = r.discover_sessions().expect("native discovery remains usable");
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
    fn discover_sessions_failopen_marks_empty_inventory_incomplete() {
        let config = SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        };
        let result = discover_sessions_failopen(&config);
        assert!(result.entries.is_empty());
        assert!(!result.is_complete());
        assert!(result.incomplete.contains(&SessionDiscoveryIncomplete {
            source: SessionDiscoverySource::Casr,
            reason: SessionDiscoveryIncompleteReason::Unavailable,
        }));
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
            timeout_secs: 120,
            dry_run: true,
        };
        let r = SessionResumer::new(c);
        assert_eq!(r.config().casr_binary, "/opt/bin/casr");
        assert_eq!(
            r.config().working_dir.as_deref(),
            Some(Path::new("/my/project"))
        );
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
    fn resumer_discover_for_provider_preserves_partial_evidence() {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "/nonexistent/casr-binary-that-does-not-exist".into(),
            ..Default::default()
        });
        let report = r
            .discover_sessions_for_provider(&AgentProvider::Codex)
            .expect("provider filtering keeps the partial report");
        assert!(report.entries.is_empty());
        assert!(!report.is_complete());
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
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: "rustc".into(),
            working_dir: Some(PathBuf::from("/definitely/nonexistent/casr-working-dir")),
            ..Default::default()
        });

        let report = r
            .discover_sessions()
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
            .run_casr(&[
                "-c",
                "printf 'abc' >&2; while :; do sleep 1; done",
            ])
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
            .run_casr(&[
                "-c",
                "printf 'raw-child-output-canary' >&2; exit 19",
            ])
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
            .run_casr_with_options_and_cancellation(
                &["-c", "sleep 10"],
                true,
                Some(&cancellation),
            )
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
}
