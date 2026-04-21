//! Wrapper for the `cass` CLI (session search/query JSON parsing with safety).
//!
//! This module treats cass (coding_agent_session_search) as an external
//! "truth source" for session data. It provides a small, typed API with:
//! - hard timeouts
//! - output size limits
//! - JSON parsing with redacted error previews
//! - version-tolerant parsing (ignores unknown fields)

use crate::agent_provider::AgentProvider;
use crate::error::Remediation;
use crate::policy::Redactor;
use crate::runtime_compat::process::Command;
#[cfg(feature = "cass-export")]
use crate::storage::{AgentSessionRecord, ExportQuery, Segment, SegmentScanQuery, StorageHandle};
use crate::suggestions::Platform;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// Supported cass agents for filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CassAgent {
    Codex,
    ClaudeCode,
    Gemini,
    Cursor,
    Aider,
    ChatGpt,
}

impl CassAgent {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor",
            Self::Aider => "aider",
            Self::ChatGpt => "chatgpt",
        }
    }

    #[must_use]
    pub fn from_provider(provider: &AgentProvider) -> Option<Self> {
        match provider {
            AgentProvider::Codex => Some(Self::Codex),
            AgentProvider::Claude => Some(Self::ClaudeCode),
            AgentProvider::Gemini => Some(Self::Gemini),
            AgentProvider::Cursor => Some(Self::Cursor),
            AgentProvider::Aider => Some(Self::Aider),
            AgentProvider::Unknown(slug) if Self::is_chatgpt_slug(slug) => Some(Self::ChatGpt),
            _ => None,
        }
    }

    #[must_use]
    pub fn to_provider(self) -> AgentProvider {
        match self {
            Self::Codex => AgentProvider::Codex,
            Self::ClaudeCode => AgentProvider::Claude,
            Self::Gemini => AgentProvider::Gemini,
            Self::Cursor => AgentProvider::Cursor,
            Self::Aider => AgentProvider::Aider,
            Self::ChatGpt => AgentProvider::Unknown("chatgpt".to_string()),
        }
    }

    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        let provider = AgentProvider::from_slug(slug.trim());
        Self::from_provider(&provider)
    }

    fn is_chatgpt_slug(slug: &str) -> bool {
        matches!(
            slug.trim().to_ascii_lowercase().as_str(),
            "chatgpt" | "chat_gpt" | "chat-gpt"
        )
    }
}

impl std::fmt::Display for CassAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Parsed output for `cass search`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassSearchResult {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub count: Option<usize>,
    #[serde(default)]
    pub total_matches: Option<usize>,
    #[serde(default)]
    pub hits: Vec<CassSearchHit>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub hits_clamped: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A single search hit from cass.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassSearchHit {
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub line_number: Option<usize>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Parsed cass session details.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassSession {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub ended_at: Option<String>,
    #[serde(default)]
    pub messages: Vec<CassMessage>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A cass message entry for a session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassMessage {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub token_count: Option<u64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Aggregated summary for a cass session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassSessionSummary {
    #[serde(default)]
    pub total_tokens: Option<i64>,
    #[serde(default)]
    pub input_tokens: Option<i64>,
    #[serde(default)]
    pub output_tokens: Option<i64>,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub session_started_at_ms: Option<i64>,
    #[serde(default)]
    pub session_ended_at_ms: Option<i64>,
    #[serde(default)]
    pub first_message_at_ms: Option<i64>,
    #[serde(default)]
    pub last_message_at_ms: Option<i64>,
}

impl CassSessionSummary {
    /// Build a summary from a cass session payload.
    #[must_use]
    pub fn from_session(session: &CassSession) -> Self {
        let mut total_tokens: i64 = 0;
        let mut input_tokens: i64 = 0;
        let mut output_tokens: i64 = 0;
        let mut has_tokens = false;

        let mut first_message_at_ms: Option<i64> = None;
        let mut last_message_at_ms: Option<i64> = None;

        for message in &session.messages {
            if let Some(timestamp) = message.timestamp.as_deref() {
                if let Some(parsed) = parse_cass_timestamp_ms(timestamp) {
                    first_message_at_ms =
                        Some(first_message_at_ms.map_or(parsed, |prev| prev.min(parsed)));
                    last_message_at_ms =
                        Some(last_message_at_ms.map_or(parsed, |prev| prev.max(parsed)));
                }
            }

            if let Some(tokens) = message.token_count {
                has_tokens = true;
                let tokens_i64 = i64::try_from(tokens).unwrap_or(i64::MAX);
                total_tokens = total_tokens.saturating_add(tokens_i64);
                if let Some(role) = message.role.as_deref() {
                    match classify_role(role) {
                        RoleClass::Input => {
                            input_tokens = input_tokens.saturating_add(tokens_i64);
                        }
                        RoleClass::Output => {
                            output_tokens = output_tokens.saturating_add(tokens_i64);
                        }
                        RoleClass::Unknown => {}
                    }
                }
            }
        }

        let session_started_at_ms = session
            .started_at
            .as_deref()
            .and_then(parse_cass_timestamp_ms);
        let session_ended_at_ms = session
            .ended_at
            .as_deref()
            .and_then(parse_cass_timestamp_ms);

        Self {
            total_tokens: has_tokens.then_some(total_tokens),
            input_tokens: has_tokens.then_some(input_tokens),
            output_tokens: has_tokens.then_some(output_tokens),
            message_count: session.messages.len(),
            session_started_at_ms,
            session_ended_at_ms,
            first_message_at_ms,
            last_message_at_ms,
        }
    }
}

/// Parsed output for `cass view` (session query).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassViewResult {
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub line_number: Option<usize>,
    #[serde(default)]
    pub context_before: Option<Vec<CassContextLine>>,
    #[serde(default)]
    pub match_line: Option<CassContextLine>,
    #[serde(default)]
    pub context_after: Option<Vec<CassContextLine>>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A context line from cass view.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassContextLine {
    #[serde(default)]
    pub line_number: Option<usize>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Parsed output for `cass index`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassIndexResult {
    /// Number of sessions indexed in this run.
    #[serde(default)]
    pub sessions_indexed: Option<usize>,
    /// Number of new sessions discovered.
    #[serde(default)]
    pub new_sessions: Option<usize>,
    /// Whether the index operation completed successfully.
    #[serde(default)]
    pub success: Option<bool>,
    /// Duration of the index operation in milliseconds.
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Parsed output for `cass status`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CassStatus {
    #[serde(default)]
    pub healthy: Option<bool>,
    #[serde(default)]
    pub index_path: Option<String>,
    #[serde(default)]
    pub total_sessions: Option<usize>,
    #[serde(default)]
    pub total_lines: Option<usize>,
    #[serde(default)]
    pub last_indexed: Option<String>,
    #[serde(default)]
    pub stale: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Errors produced by the cass wrapper.
#[derive(thiserror::Error, Debug)]
pub enum CassError {
    #[error("cass is not installed or not found on PATH")]
    NotInstalled,
    #[error("cass timed out after {timeout_secs}s")]
    Timeout { timeout_secs: u64 },
    #[error("cass failed with exit code {status}: {stderr}")]
    NonZeroExit { status: i32, stderr: String },
    #[error("cass output exceeded {max_bytes} bytes")]
    OutputTooLarge { bytes: usize, max_bytes: usize },
    #[error("cass returned invalid JSON: {message}")]
    InvalidJson { message: String, preview: String },
    /// Reserved for future use when cass CLI returns explicit "no results" errors.
    /// Currently, empty search results are not treated as errors (the query succeeded
    /// with zero hits). This variant exists for API completeness and future CLI changes.
    #[error("cass returned no results for query")]
    NoResults { query: String },
    #[error("cass I/O error: {message}")]
    Io { message: String },
}

impl CassError {
    /// Optional remediation guidance for this error.
    #[must_use]
    pub fn remediation(&self) -> Remediation {
        match self {
            Self::NotInstalled => {
                let mut remediation =
                    Remediation::new("Install cass and ensure it is available on PATH.")
                        .command("Verify install", "cass --version");
                if let Some(cmd) = Platform::detect().install_command("cass") {
                    remediation = remediation.command("Install cass", cmd);
                }
                remediation.alternative("If cass is installed elsewhere, add it to PATH.")
            }
            Self::Timeout { timeout_secs } => Remediation::new(format!(
                "cass did not respond within {timeout_secs}s. Retry or check system load."
            ))
            .command("Retry search", "cass search \"query\" --robot --limit 5")
            .alternative("Increase the timeout for cass commands."),
            Self::NonZeroExit { .. } => Remediation::new(
                "cass exited with an error. Check cass logs or rerun with verbose output.",
            )
            .command("Check status", "cass status --json")
            .alternative("Run cass diag --json for detailed diagnostics."),
            Self::OutputTooLarge { .. } => {
                Remediation::new("cass output was too large. Reduce limit or use --fields minimal.")
                    .command(
                        "Smaller query",
                        "cass search \"query\" --robot --limit 5 --fields minimal",
                    )
                    .alternative("Use aggregation to reduce output size.")
            }
            Self::InvalidJson { .. } => Remediation::new(
                "cass returned malformed JSON. Upgrade cass or verify output format.",
            )
            .command("Check cass version", "cass --version")
            .alternative("Report the issue with a redacted output sample."),
            Self::NoResults { query } => Remediation::new(format!(
                "No sessions matched query: {query}. Broaden search or check index."
            ))
            .command("Check index", "cass status --json")
            .alternative("Try a broader search term."),
            Self::Io { .. } => {
                Remediation::new("I/O error while running cass. Check permissions and retry.")
                    .alternative("Verify cass binary permissions.")
            }
        }
    }
}

/// Options for cass search.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub agent: Option<CassAgent>,
    pub workspace: Option<String>,
    pub days: Option<u32>,
    pub fields: Option<String>,
    pub max_tokens: Option<usize>,
}

/// Options for cass view (query).
#[derive(Debug, Clone, Default)]
pub struct ViewOptions {
    pub context_lines: Option<usize>,
}

// Canonical value in TuningConfig::SearchTuning.
#[cfg(feature = "cass-export")]
const DEFAULT_CASS_EXPORT_LIMIT: usize =
    crate::tuning_config::SearchTuning::DEFAULT_CASS_EXPORT_LIMIT;
#[cfg(feature = "cass-export")]
const CASS_EXPORT_SESSION_PREFIX: &str = "ft-session-";

/// Query controls for exporting FrankenTerm recorder sessions to cass connectors.
#[cfg(feature = "cass-export")]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CassExportQuery {
    /// Only return sessions whose recorder row id is greater than this value.
    pub after_id: Option<i64>,
    /// Filter by pane id.
    pub pane_id: Option<u64>,
    /// Filter by session start timestamp lower bound (epoch ms).
    pub since: Option<i64>,
    /// Filter by session start timestamp upper bound (epoch ms).
    pub until: Option<i64>,
    /// Maximum number of exported sessions.
    pub limit: usize,
}

#[cfg(feature = "cass-export")]
impl CassExportQuery {
    fn effective_limit(&self) -> usize {
        if self.limit == 0 {
            DEFAULT_CASS_EXPORT_LIMIT
        } else {
            self.limit
        }
    }
}

#[cfg(feature = "cass-export")]
impl Default for CassExportQuery {
    fn default() -> Self {
        Self {
            after_id: None,
            pane_id: None,
            since: None,
            until: None,
            limit: DEFAULT_CASS_EXPORT_LIMIT,
        }
    }
}

/// Query controls for exporting session content chunks to cass connectors.
#[cfg(feature = "cass-export")]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CassContentExportQuery {
    /// Only return segments with id strictly greater than this value.
    pub after_id: Option<i64>,
    /// Maximum number of content chunks to export.
    pub limit: usize,
}

#[cfg(feature = "cass-export")]
impl CassContentExportQuery {
    fn effective_limit(&self) -> usize {
        if self.limit == 0 {
            DEFAULT_CASS_EXPORT_LIMIT
        } else {
            self.limit
        }
    }
}

#[cfg(feature = "cass-export")]
impl Default for CassContentExportQuery {
    fn default() -> Self {
        Self {
            after_id: None,
            limit: DEFAULT_CASS_EXPORT_LIMIT,
        }
    }
}

/// Cass-facing summary of a FrankenTerm agent session.
#[cfg(feature = "cass-export")]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CassExportSessionRecord {
    /// Stable recorder row id for the session.
    pub session_row_id: i64,
    /// Export identifier. Uses the agent session id when present, otherwise
    /// falls back to `ft-session-<row_id>`.
    pub session_id: String,
    /// Agent provider/classification from the recorder session.
    pub agent_type: String,
    /// Workspace/cwd associated with the pane at export time.
    pub workspace: Option<String>,
    /// Pane ids participating in the session. Currently single-pane, but kept
    /// as a vector to match the connector-facing contract.
    pub pane_ids: Vec<u64>,
    /// Session start timestamp (epoch ms).
    pub started_at_ms: i64,
    /// Session end timestamp (epoch ms) if the session has completed.
    pub ended_at_ms: Option<i64>,
    /// Model name recorded for the agent session, if available.
    pub model_name: Option<String>,
    /// Export-time content token estimate. Prefers recorded token totals when present.
    pub content_tokens: u64,
    /// External correlation id (for example a cass correlation result), if present.
    pub external_id: Option<String>,
    /// External correlation metadata persisted on the session, if present.
    pub external_meta: Option<Value>,
}

/// Cass-facing content chunk exported from recorder segments.
#[cfg(feature = "cass-export")]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CassExportContentChunk {
    /// Stable recorder row id for the session.
    pub session_row_id: i64,
    /// Export identifier for the session.
    pub session_id: String,
    /// Recorder segment id.
    pub segment_id: i64,
    /// Pane id that produced the content.
    pub pane_id: u64,
    /// Per-pane sequence number for the segment.
    pub seq: u64,
    /// Timestamp when the content was captured (epoch ms).
    pub timestamp_ms: i64,
    /// Terminal content captured for indexing.
    pub content: String,
    /// Content class. Recorder segments are terminal output today.
    pub content_type: String,
}

/// Export recorder sessions for cass connector ingestion.
#[cfg(feature = "cass-export")]
pub async fn export_sessions(
    storage: &StorageHandle,
    query: &CassExportQuery,
) -> crate::Result<Vec<CassExportSessionRecord>> {
    let pane_workspaces: HashMap<u64, String> = storage
        .get_panes()
        .await?
        .into_iter()
        .filter_map(|pane| pane.cwd.map(|cwd| (pane.pane_id, cwd)))
        .collect();

    let mut sessions = storage
        .export_sessions(ExportQuery {
            pane_id: query.pane_id,
            since: query.since,
            until: query.until,
            limit: None,
        })
        .await?;
    sessions.sort_unstable_by_key(|session| session.id);

    let mut exported = Vec::new();
    for session in sessions
        .into_iter()
        .filter(|session| query.after_id.is_none_or(|after_id| session.id > after_id))
        .take(query.effective_limit())
    {
        let content_tokens = match session_recorded_tokens(&session) {
            Some(tokens) => tokens,
            None => estimate_session_content_tokens(storage, &session).await?,
        };
        exported.push(CassExportSessionRecord {
            session_row_id: session.id,
            session_id: export_session_identifier(&session),
            agent_type: session.agent_type.clone(),
            workspace: pane_workspaces.get(&session.pane_id).cloned(),
            pane_ids: vec![session.pane_id],
            started_at_ms: session.started_at,
            ended_at_ms: session.ended_at,
            model_name: session.model_name.clone(),
            content_tokens,
            external_id: session.external_id.clone(),
            external_meta: session.external_meta.clone(),
        });
    }

    Ok(exported)
}

/// ft-xbnl0.2.3 Cx-first sibling of [`export_sessions`].
///
/// Tick 151: rewritten from a pre-flight delegate into a fully
/// cx-threaded body — every storage call is routed through its
/// `_with_cx` sibling, including the per-session token estimate
/// in the inner loop. The cass connector can have many concurrent
/// export batches in flight; a cancelled parent cx now bails at
/// the earliest boundary *and* at every storage seam inside the
/// export loop.
#[cfg(feature = "cass-export")]
pub async fn export_sessions_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    query: &CassExportQuery,
) -> crate::Result<Vec<CassExportSessionRecord>> {
    cx.checkpoint()
        .map_err(|err| crate::Error::Runtime(format!("cass export_sessions cancelled: {err}")))?;

    let pane_workspaces: HashMap<u64, String> = storage
        .get_panes_with_cx(cx)
        .await?
        .into_iter()
        .filter_map(|pane| pane.cwd.map(|cwd| (pane.pane_id, cwd)))
        .collect();

    let mut sessions = storage
        .export_sessions_with_cx(
            cx,
            ExportQuery {
                pane_id: query.pane_id,
                since: query.since,
                until: query.until,
                limit: None,
            },
        )
        .await?;
    sessions.sort_unstable_by_key(|session| session.id);

    let mut exported = Vec::new();
    for session in sessions
        .into_iter()
        .filter(|session| query.after_id.is_none_or(|after_id| session.id > after_id))
        .take(query.effective_limit())
    {
        let content_tokens = match session_recorded_tokens(&session) {
            Some(tokens) => tokens,
            None => estimate_session_content_tokens_with_cx(cx, storage, &session).await?,
        };
        exported.push(CassExportSessionRecord {
            session_row_id: session.id,
            session_id: export_session_identifier(&session),
            agent_type: session.agent_type.clone(),
            workspace: pane_workspaces.get(&session.pane_id).cloned(),
            pane_ids: vec![session.pane_id],
            started_at_ms: session.started_at,
            ended_at_ms: session.ended_at,
            model_name: session.model_name.clone(),
            content_tokens,
            external_id: session.external_id.clone(),
            external_meta: session.external_meta.clone(),
        });
    }

    Ok(exported)
}

/// Export recorder content chunks for a specific session.
#[cfg(feature = "cass-export")]
pub async fn export_content(
    storage: &StorageHandle,
    session_id: &str,
    query: &CassContentExportQuery,
) -> crate::Result<Vec<CassExportContentChunk>> {
    let session = resolve_export_session(storage, session_id).await?;
    let exported_session_id = export_session_identifier(&session);
    let segments = storage
        .scan_segments(SegmentScanQuery {
            after_id: query.after_id,
            pane_id: Some(session.pane_id),
            since: Some(session.started_at),
            until: session.ended_at,
            limit: query.effective_limit(),
        })
        .await?;

    Ok(segments
        .into_iter()
        .map(|segment| CassExportContentChunk {
            session_row_id: session.id,
            session_id: exported_session_id.clone(),
            segment_id: segment.id,
            pane_id: segment.pane_id,
            seq: segment.seq,
            timestamp_ms: segment.captured_at,
            content: segment.content,
            content_type: "output".to_string(),
        })
        .collect())
}

/// ft-xbnl0.2.3 Cx-first sibling of [`export_content`].
///
/// Tick 151: rewritten from a pre-flight delegate into a fully
/// cx-threaded body — session resolution routes through
/// `resolve_export_session_with_cx` and the segment scan uses
/// `scan_segments_with_cx`. Used by the cass connector's
/// per-session chunk-fetch loop.
#[cfg(feature = "cass-export")]
pub async fn export_content_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    session_id: &str,
    query: &CassContentExportQuery,
) -> crate::Result<Vec<CassExportContentChunk>> {
    cx.checkpoint()
        .map_err(|err| crate::Error::Runtime(format!("cass export_content cancelled: {err}")))?;

    let session = resolve_export_session_with_cx(cx, storage, session_id).await?;
    let exported_session_id = export_session_identifier(&session);
    let segments = storage
        .scan_segments_with_cx(
            cx,
            SegmentScanQuery {
                after_id: query.after_id,
                pane_id: Some(session.pane_id),
                since: Some(session.started_at),
                until: session.ended_at,
                limit: query.effective_limit(),
            },
        )
        .await?;

    Ok(segments
        .into_iter()
        .map(|segment| CassExportContentChunk {
            session_row_id: session.id,
            session_id: exported_session_id.clone(),
            segment_id: segment.id,
            pane_id: segment.pane_id,
            seq: segment.seq,
            timestamp_ms: segment.captured_at,
            content: segment.content,
            content_type: "output".to_string(),
        })
        .collect())
}

/// Thin wrapper around the cass CLI.
#[derive(Debug, Clone)]
pub struct CassClient {
    binary: String,
    timeout: Duration,
    max_output_bytes: usize,
    max_error_bytes: usize,
}

impl Default for CassClient {
    fn default() -> Self {
        Self {
            binary: "cass".to_string(),
            timeout: Duration::from_secs(15),
            max_output_bytes: 512 * 1024,
            max_error_bytes: 8 * 1024,
        }
    }
}

impl CassClient {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    #[must_use]
    pub fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout = Duration::from_secs(timeout_secs);
        self
    }

    #[must_use]
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    #[must_use]
    pub fn with_max_error_bytes(mut self, max_error_bytes: usize) -> Self {
        self.max_error_bytes = max_error_bytes;
        self
    }

    /// Search sessions via `cass search`.
    pub async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<CassSearchResult, CassError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.search_with_cx(&cx, query, options).await
    }

    /// Search sessions via `cass search` under an explicit `&Cx`.
    ///
    /// Cx-first entry point (ft-xbnl0.2.3): the subprocess timeout binds
    /// to the provided `Cx` via [`crate::runtime_compat::timeout_with_cx`]
    /// so cancellation, budget, and virtual time propagate into the
    /// `cass` invocation — matching the pattern established in
    /// [`caut::CautClient::usage_cx`].
    ///
    /// The legacy [`search`](Self::search) entry point is preserved for
    /// non-migrated callers; under `asupersync-runtime` it now delegates
    /// to this method with an ambient Cx.
    pub async fn search_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: &str,
        options: &SearchOptions,
    ) -> Result<CassSearchResult, CassError> {
        let args = build_search_args(query, options);
        let output = self.run_with_cx(cx, &args).await?;
        parse_json(&output, self.max_error_bytes)
    }

    /// Search cass for sessions under a given path (and optional agent filter).
    pub async fn search_sessions(
        &self,
        path: &Path,
        agent: Option<CassAgent>,
    ) -> Result<Vec<CassSession>, CassError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.search_sessions_with_cx(&cx, path, agent).await
    }

    /// Search sessions under a given path + agent under an explicit `&Cx`
    /// (ft-xbnl0.2.3 Cx-first entry point).
    pub async fn search_sessions_with_cx(
        &self,
        cx: &crate::cx::Cx,
        path: &Path,
        agent: Option<CassAgent>,
    ) -> Result<Vec<CassSession>, CassError> {
        let args = build_search_sessions_args(path, agent);
        let output = self.run_with_cx(cx, &args).await?;
        let sessions = parse_sessions(&output, self.max_error_bytes)?;
        if sessions.is_empty() {
            return Err(CassError::NoResults {
                query: format!("path={}", path.display()),
            });
        }
        Ok(sessions)
    }

    /// Query a specific session by session id.
    pub async fn query_session(&self, session_id: &str) -> Result<CassSession, CassError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.query_session_with_cx(&cx, session_id).await
    }

    /// Query a specific session by session id under an explicit `&Cx`
    /// (ft-xbnl0.2.3 Cx-first entry point).
    pub async fn query_session_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session_id: &str,
    ) -> Result<CassSession, CassError> {
        let args = build_query_session_args(session_id);
        let output = self.run_with_cx(cx, &args).await?;
        let mut sessions = parse_sessions(&output, self.max_error_bytes)?;
        if let Some(session) = sessions.pop() {
            return Ok(session);
        }
        Err(CassError::NoResults {
            query: format!("session_id={session_id}"),
        })
    }

    /// Query a specific session via `cass view`.
    pub async fn query(
        &self,
        session_path: &Path,
        line_number: usize,
        options: &ViewOptions,
    ) -> Result<CassViewResult, CassError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self
            .query_with_cx(&cx, session_path, line_number, options)
            .await
    }

    /// Query a specific session via `cass view` under an explicit `&Cx`
    /// (ft-xbnl0.2.3 Cx-first entry point).
    pub async fn query_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session_path: &Path,
        line_number: usize,
        options: &ViewOptions,
    ) -> Result<CassViewResult, CassError> {
        let args = build_query_args(session_path, line_number, options);
        let output = self.run_with_cx(cx, &args).await?;
        parse_json(&output, self.max_error_bytes)
    }

    /// Check cass health via `cass status`.
    pub async fn status(&self) -> Result<CassStatus, CassError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.status_with_cx(&cx).await
    }

    /// Check cass health via `cass status` under an explicit `&Cx`
    /// (ft-xbnl0.2.3 Cx-first entry point).
    pub async fn status_with_cx(&self, cx: &crate::cx::Cx) -> Result<CassStatus, CassError> {
        let args = vec!["status".to_string(), "--json".to_string()];
        let output = self.run_with_cx(cx, &args).await?;
        parse_json(&output, self.max_error_bytes)
    }

    /// Trigger a cass index refresh via `cass index`.
    ///
    /// This updates the cass search index with any new or modified session files.
    /// Used as part of the swarm learning cycle: after an agent session completes,
    /// indexing makes the session's content searchable for future agents.
    ///
    /// Returns the raw JSON output from `cass index --json` on success.
    pub async fn trigger_index(
        &self,
        workspace: Option<&str>,
    ) -> Result<CassIndexResult, CassError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.trigger_index_with_cx(&cx, workspace).await
    }

    /// Trigger a cass index refresh under an explicit `&Cx`
    /// (ft-xbnl0.2.3 Cx-first entry point).
    pub async fn trigger_index_with_cx(
        &self,
        cx: &crate::cx::Cx,
        workspace: Option<&str>,
    ) -> Result<CassIndexResult, CassError> {
        let args = build_trigger_index_args(workspace);
        let output = self.run_with_cx(cx, &args).await?;
        parse_json(&output, self.max_error_bytes)
    }

    /// Cx-first subprocess execution (ft-xbnl0.2.3). The subprocess
    /// timeout is bound to the provided `Cx` via
    /// [`crate::runtime_compat::timeout_with_cx`] so cancellation,
    /// budget, and virtual time propagate into the `cass` invocation.
    /// Mirrors the pattern in [`caut::CautClient::run_with_cx`].
    async fn run_with_cx(&self, cx: &crate::cx::Cx, args: &[String]) -> Result<String, CassError> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(args);
        cmd.kill_on_drop(true);

        let output =
            match crate::runtime_compat::timeout_with_cx(cx, self.timeout, cmd.output()).await {
                Ok(result) => result.map_err(|err| categorize_io_error(&err))?,
                Err(_) => {
                    return Err(CassError::Timeout {
                        timeout_secs: self.timeout.as_secs(),
                    });
                }
            };

        self.finalize_output(output)
    }

    /// Shared post-timeout validation: checks exit status, stderr, and
    /// stdout size. Extracted so `run` and `run_with_cx` share the same
    /// error surface bit-for-bit.
    fn finalize_output(&self, output: std::process::Output) -> Result<String, CassError> {
        if !output.status.success() {
            let status = output.status.code().unwrap_or(-1);
            let stderr_bytes = if output.stderr.len() > self.max_error_bytes {
                &output.stderr[..self.max_error_bytes]
            } else {
                &output.stderr
            };
            let stderr = String::from_utf8_lossy(stderr_bytes).to_string();
            let stderr_preview = redact_and_truncate(&stderr, self.max_error_bytes);
            return Err(CassError::NonZeroExit {
                status,
                stderr: stderr_preview,
            });
        }

        // Check raw byte length BEFORE allocating string to prevent OOM on huge outputs
        let raw_bytes = output.stdout.len();
        if raw_bytes > self.max_output_bytes {
            return Err(CassError::OutputTooLarge {
                bytes: raw_bytes,
                max_bytes: self.max_output_bytes,
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// Build cass search subcommand args. Extracted so `search` and
/// `search_with_cx` share the argv construction bit-for-bit.
fn build_search_args(query: &str, options: &SearchOptions) -> Vec<String> {
    let mut args = vec![
        "search".to_string(),
        query.to_string(),
        "--robot".to_string(),
    ];

    if let Some(limit) = options.limit {
        args.push("--limit".to_string());
        args.push(limit.to_string());
    }
    if let Some(offset) = options.offset {
        args.push("--offset".to_string());
        args.push(offset.to_string());
    }
    if let Some(agent) = &options.agent {
        args.push("--agent".to_string());
        args.push(agent.as_str().to_string());
    }
    if let Some(workspace) = &options.workspace {
        args.push("--workspace".to_string());
        args.push(workspace.clone());
    }
    if let Some(days) = options.days {
        args.push("--days".to_string());
        args.push(days.to_string());
    }
    if let Some(fields) = &options.fields {
        args.push("--fields".to_string());
        args.push(fields.clone());
    }
    if let Some(max_tokens) = options.max_tokens {
        args.push("--max-tokens".to_string());
        args.push(max_tokens.to_string());
    }

    args
}

/// Build cass search-by-path args. Extracted so `search_sessions` and
/// `search_sessions_with_cx` share the argv construction bit-for-bit.
fn build_search_sessions_args(path: &Path, agent: Option<CassAgent>) -> Vec<String> {
    let mut args = vec![
        "search".to_string(),
        "--path".to_string(),
        path.to_string_lossy().to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];

    if let Some(agent) = agent {
        args.push("--agent".to_string());
        args.push(agent.as_str().to_string());
    }

    args
}

/// Build cass query-by-session-id args. Extracted so `query_session` and
/// `query_session_with_cx` share the argv construction bit-for-bit.
fn build_query_session_args(session_id: &str) -> Vec<String> {
    vec![
        "query".to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]
}

/// Build cass view args. Extracted so `query` and `query_with_cx` share
/// the argv construction bit-for-bit.
fn build_query_args(session_path: &Path, line_number: usize, options: &ViewOptions) -> Vec<String> {
    let mut args = vec![
        "view".to_string(),
        session_path.to_string_lossy().to_string(),
        "-n".to_string(),
        line_number.to_string(),
        "--json".to_string(),
    ];

    if let Some(context) = options.context_lines {
        args.push("-C".to_string());
        args.push(context.to_string());
    }

    args
}

/// Build cass index-refresh args. Extracted so `trigger_index` and
/// `trigger_index_with_cx` share the argv construction bit-for-bit.
fn build_trigger_index_args(workspace: Option<&str>) -> Vec<String> {
    let mut args = vec!["index".to_string(), "--json".to_string()];
    if let Some(ws) = workspace {
        args.push("--path".to_string());
        args.push(ws.to_string());
    }
    args
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleClass {
    Input,
    Output,
    Unknown,
}

fn classify_role(role: &str) -> RoleClass {
    match role.trim().to_lowercase().as_str() {
        "user" | "human" | "system" => RoleClass::Input,
        "assistant" | "model" | "ai" => RoleClass::Output,
        _ => RoleClass::Unknown,
    }
}

/// Parse a cass timestamp into epoch milliseconds.
///
/// Accepts epoch seconds, epoch milliseconds, RFC3339, and common
/// `%Y-%m-%d %H:%M:%S` formats.
pub fn parse_cass_timestamp_ms(raw: &str) -> Option<i64> {
    use chrono::{DateTime, NaiveDateTime, Utc};

    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    if let Ok(value) = raw.parse::<i64>() {
        // Heuristic: treat large values as ms, smaller as seconds.
        return Some(if value > 10_000_000_000 {
            value
        } else {
            value.saturating_mul(1_000)
        });
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.timestamp_millis());
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc).timestamp_millis());
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%SZ") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc).timestamp_millis());
    }

    None
}

fn categorize_io_error(err: &std::io::Error) -> CassError {
    match err.kind() {
        std::io::ErrorKind::NotFound => CassError::NotInstalled,
        _ => CassError::Io {
            message: err.to_string(),
        },
    }
}

fn parse_json<T: DeserializeOwned>(input: &str, max_preview: usize) -> Result<T, CassError> {
    serde_json::from_str(input).map_err(|err| CassError::InvalidJson {
        message: err.to_string(),
        preview: redact_and_truncate(input, max_preview),
    })
}

fn parse_sessions(input: &str, max_preview: usize) -> Result<Vec<CassSession>, CassError> {
    let value: Value = serde_json::from_str(input).map_err(|err| CassError::InvalidJson {
        message: err.to_string(),
        preview: redact_and_truncate(input, max_preview),
    })?;

    if let Some(array) = value.as_array() {
        return serde_json::from_value(Value::Array(array.clone())).map_err(|err| {
            CassError::InvalidJson {
                message: err.to_string(),
                preview: redact_and_truncate(input, max_preview),
            }
        });
    }

    if let Some(sessions_val) = value.get("sessions") {
        return serde_json::from_value(sessions_val.clone()).map_err(|err| {
            CassError::InvalidJson {
                message: err.to_string(),
                preview: redact_and_truncate(input, max_preview),
            }
        });
    }

    serde_json::from_value(value)
        .map(|session: CassSession| vec![session])
        .map_err(|err| CassError::InvalidJson {
            message: err.to_string(),
            preview: redact_and_truncate(input, max_preview),
        })
}

#[cfg(feature = "cass-export")]
fn export_session_identifier(session: &AgentSessionRecord) -> String {
    session
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{CASS_EXPORT_SESSION_PREFIX}{}", session.id))
}

#[cfg(feature = "cass-export")]
fn parse_export_session_row_id(session_id: &str) -> Option<i64> {
    session_id
        .trim()
        .strip_prefix(CASS_EXPORT_SESSION_PREFIX)?
        .parse::<i64>()
        .ok()
}

#[cfg(feature = "cass-export")]
fn session_recorded_tokens(session: &AgentSessionRecord) -> Option<u64> {
    session
        .total_tokens
        .and_then(|value| u64::try_from(value).ok())
}

#[cfg(feature = "cass-export")]
async fn estimate_session_content_tokens(
    storage: &StorageHandle,
    session: &AgentSessionRecord,
) -> crate::Result<u64> {
    let segments = storage
        .export_segments(ExportQuery {
            pane_id: Some(session.pane_id),
            since: Some(session.started_at),
            until: session.ended_at,
            limit: None,
        })
        .await?;
    Ok(segments.iter().map(estimate_segment_tokens).sum())
}

/// ft-xbnl0.2.3 Cx-first sibling of [`estimate_session_content_tokens`].
///
/// Routes through `export_segments_with_cx` so the session-token
/// estimate honours cancellation of the parent export loop.
#[cfg(feature = "cass-export")]
async fn estimate_session_content_tokens_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    session: &AgentSessionRecord,
) -> crate::Result<u64> {
    cx.checkpoint().map_err(|err| {
        crate::Error::Runtime(format!(
            "cass estimate_session_content_tokens cancelled: {err}"
        ))
    })?;
    let segments = storage
        .export_segments_with_cx(
            cx,
            ExportQuery {
                pane_id: Some(session.pane_id),
                since: Some(session.started_at),
                until: session.ended_at,
                limit: None,
            },
        )
        .await?;
    Ok(segments.iter().map(estimate_segment_tokens).sum())
}

#[cfg(feature = "cass-export")]
fn estimate_segment_tokens(segment: &Segment) -> u64 {
    estimate_text_tokens(&segment.content)
}

#[cfg(feature = "cass-export")]
fn estimate_text_tokens(content: &str) -> u64 {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return 0;
    }

    if trimmed.chars().any(char::is_whitespace) {
        u64::try_from(trimmed.split_whitespace().count()).unwrap_or(u64::MAX)
    } else {
        let chars = u64::try_from(trimmed.chars().count()).unwrap_or(u64::MAX);
        chars.saturating_add(3) / 4
    }
}

#[cfg(feature = "cass-export")]
const RESOLVE_SESSION_FALLBACK_LIMIT: usize = 10_000;

#[cfg(feature = "cass-export")]
async fn resolve_export_session(
    storage: &StorageHandle,
    session_id: &str,
) -> crate::Result<AgentSessionRecord> {
    let requested_id = session_id.trim();

    if let Some(row_id) = parse_export_session_row_id(session_id) {
        if let Some(session) = storage.get_agent_session(row_id).await? {
            if export_session_identifier(&session) == requested_id {
                return Ok(session);
            }
        }
    }

    // Bounded fallback: scan at most RESOLVE_SESSION_FALLBACK_LIMIT sessions
    // to prevent OOM on large databases. If the session isn't in the most
    // recent N sessions, it's effectively gone.
    let sessions = storage
        .export_sessions(ExportQuery {
            limit: Some(RESOLVE_SESSION_FALLBACK_LIMIT),
            ..ExportQuery::default()
        })
        .await?;

    sessions
        .into_iter()
        .find(|candidate| export_session_identifier(candidate) == requested_id)
        .ok_or_else(|| {
            crate::Error::Storage(crate::StorageError::NotFound(format!(
                "cass export session '{requested_id}'"
            )))
        })
}

/// ft-xbnl0.2.3 Cx-first sibling of [`resolve_export_session`].
///
/// Routes both the direct `get_agent_session_with_cx` probe and the
/// bounded `export_sessions_with_cx` fallback through cx-first
/// storage siblings so a cancelled parent interrupts the scan.
#[cfg(feature = "cass-export")]
async fn resolve_export_session_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    session_id: &str,
) -> crate::Result<AgentSessionRecord> {
    cx.checkpoint().map_err(|err| {
        crate::Error::Runtime(format!("cass resolve_export_session cancelled: {err}"))
    })?;
    let requested_id = session_id.trim();

    if let Some(row_id) = parse_export_session_row_id(session_id) {
        if let Some(session) = storage.get_agent_session_with_cx(cx, row_id).await? {
            if export_session_identifier(&session) == requested_id {
                return Ok(session);
            }
        }
    }

    let sessions = storage
        .export_sessions_with_cx(
            cx,
            ExportQuery {
                limit: Some(RESOLVE_SESSION_FALLBACK_LIMIT),
                ..ExportQuery::default()
            },
        )
        .await?;

    sessions
        .into_iter()
        .find(|candidate| export_session_identifier(candidate) == requested_id)
        .ok_or_else(|| {
            crate::Error::Storage(crate::StorageError::NotFound(format!(
                "cass export session '{requested_id}'"
            )))
        })
}

fn redact_and_truncate(input: &str, max_len: usize) -> String {
    let redactor = Redactor::new();
    let redacted = redactor.redact(input);
    if redacted.len() <= max_len {
        return redacted;
    }

    // `max_len` is byte-oriented (CLI/output budgets). Truncate on a UTF-8
    // boundary so the total output (including "..." ellipsis) stays within
    // the byte budget.
    let ellipsis = "...";
    if max_len < ellipsis.len() {
        // Not enough room for the ellipsis; truncate to max_len bytes.
        let mut end = max_len.min(redacted.len());
        while end > 0 && !redacted.is_char_boundary(end) {
            end -= 1;
        }
        return redacted[..end].to_string();
    }
    let budget = max_len - ellipsis.len();
    let mut end = budget.min(redacted.len());
    while end > 0 && !redacted.is_char_boundary(end) {
        end -= 1;
    }

    let mut truncated = redacted[..end].to_string();
    truncated.push_str(ellipsis);
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_search_result_basic() {
        let payload = json!({
            "query": "test",
            "limit": 10,
            "offset": 0,
            "count": 2,
            "total_matches": 2,
            "hits": [
                {
                    "source_path": "/path/to/session.jsonl",
                    "line_number": 42,
                    "agent": "codex"
                },
                {
                    "source_path": "/path/to/other.jsonl",
                    "line_number": 100,
                    "agent": "claude_code"
                }
            ]
        });

        let parsed: CassSearchResult =
            parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.query.as_deref(), Some("test"));
        assert_eq!(parsed.hits.len(), 2);
        assert_eq!(parsed.hits[0].agent.as_deref(), Some("codex"));
        assert_eq!(parsed.hits[1].line_number, Some(100));
    }

    #[test]
    fn parse_search_result_with_extras() {
        let payload = json!({
            "query": "test",
            "hits": [],
            "unknown_field": "ignored",
            "nested": { "deep": true }
        });

        let parsed: CassSearchResult =
            parse_json(&payload.to_string(), 4096).expect("should parse");
        assert!(parsed.extra.contains_key("unknown_field"));
        assert!(parsed.extra.contains_key("nested"));
    }

    #[test]
    fn parse_search_hit_with_content() {
        let payload = json!({
            "query": "error",
            "hits": [
                {
                    "source_path": "/path/to/session.jsonl",
                    "line_number": 10,
                    "agent": "gemini",
                    "content": "Some error message here",
                    "timestamp": "2026-01-29T10:00:00Z",
                    "score": 0.95
                }
            ]
        });

        let parsed: CassSearchResult =
            parse_json(&payload.to_string(), 4096).expect("should parse");
        let hit = &parsed.hits[0];
        assert_eq!(hit.content.as_deref(), Some("Some error message here"));
        assert_eq!(hit.timestamp.as_deref(), Some("2026-01-29T10:00:00Z"));
        assert!((hit.score.unwrap() - 0.95).abs() < 0.001);
    }

    #[test]
    fn parse_search_empty_hits() {
        let payload = json!({
            "query": "nonexistent",
            "hits": [],
            "count": 0,
            "total_matches": 0
        });

        let parsed: CassSearchResult =
            parse_json(&payload.to_string(), 4096).expect("should parse");
        assert!(parsed.hits.is_empty());
        assert_eq!(parsed.count, Some(0));
    }

    #[test]
    fn parse_view_result_basic() {
        let payload = json!({
            "source_path": "/path/to/session.jsonl",
            "line_number": 42,
            "match_line": {
                "line_number": 42,
                "content": "The matching line content",
                "role": "assistant"
            },
            "agent": "codex"
        });

        let parsed: CassViewResult = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.line_number, Some(42));
        assert!(parsed.match_line.is_some());
        let match_line = parsed.match_line.unwrap();
        assert_eq!(match_line.role.as_deref(), Some("assistant"));
    }

    #[test]
    fn parse_sessions_array() {
        let payload = json!([
            {
                "session_id": "sess-1",
                "agent": "codex",
                "project_path": "/repo",
                "messages": [
                    { "role": "user", "content": "hi", "timestamp": "2026-01-29T12:00:00Z" }
                ]
            },
            {
                "session_id": "sess-2",
                "agent": "claude_code",
                "project_path": "/repo2"
            }
        ]);

        let parsed = parse_sessions(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(parsed[0].messages.len(), 1);
        assert_eq!(parsed[1].agent.as_deref(), Some("claude_code"));
    }

    #[test]
    fn parse_sessions_wrapped() {
        let payload = json!({
            "sessions": [
                {
                    "session_id": "sess-3",
                    "agent": "gemini",
                    "project_path": "/repo3"
                }
            ],
            "extra_field": true
        });

        let parsed = parse_sessions(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].session_id.as_deref(), Some("sess-3"));
    }

    #[test]
    fn parse_sessions_single_object() {
        let payload = json!({
            "session_id": "sess-4",
            "agent": "codex",
            "project_path": "/repo4"
        });

        let parsed = parse_sessions(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].session_id.as_deref(), Some("sess-4"));
    }

    #[test]
    fn parse_view_result_with_context() {
        let payload = json!({
            "source_path": "/path/to/session.jsonl",
            "line_number": 42,
            "context_before": [
                { "line_number": 40, "content": "before line 1" },
                { "line_number": 41, "content": "before line 2" }
            ],
            "match_line": { "line_number": 42, "content": "match" },
            "context_after": [
                { "line_number": 43, "content": "after line 1" }
            ]
        });

        let parsed: CassViewResult = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.context_before.as_ref().map(|v| v.len()), Some(2));
        assert_eq!(parsed.context_after.as_ref().map(|v| v.len()), Some(1));
    }

    #[test]
    fn parse_status_healthy() {
        let payload = json!({
            "healthy": true,
            "index_path": "/home/user/.cass/index",
            "total_sessions": 150,
            "total_lines": 50000,
            "last_indexed": "2026-01-29T10:00:00Z",
            "stale": false
        });

        let parsed: CassStatus = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.healthy, Some(true));
        assert_eq!(parsed.total_sessions, Some(150));
        assert_eq!(parsed.stale, Some(false));
    }

    #[test]
    fn parse_status_stale() {
        let payload = json!({
            "healthy": true,
            "stale": true,
            "total_sessions": 100
        });

        let parsed: CassStatus = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.stale, Some(true));
    }

    #[test]
    fn parse_minimal_valid_json() {
        let parsed: CassSearchResult = parse_json("{}", 4096).expect("should parse");
        assert!(parsed.query.is_none());
        assert!(parsed.hits.is_empty());
    }

    #[test]
    fn invalid_json_returns_preview() {
        let err = parse_json::<CassSearchResult>("{not_json}", 20).expect_err("should error");
        match err {
            CassError::InvalidJson { preview, .. } => {
                assert!(preview.contains("not_json"));
            }
            other => panic!("Unexpected error: {other:?}"),
        }
    }

    #[test]
    fn redact_and_truncate_masks_secrets() {
        let secret = "sk-abc123456789012345678901234567890123456789012345678901";
        let text = format!("token={secret}");
        let redacted = redact_and_truncate(&text, 200);
        assert!(!redacted.contains("sk-"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn redact_and_truncate_multibyte_respects_byte_budget() {
        let text = "🚀".repeat(10);
        let redacted = redact_and_truncate(&text, 5);
        assert!(redacted.ends_with("..."));
        // With max_len=5: budget is 5-3=2 bytes for content, but "🚀" is 4 bytes
        // so no emoji fits → result is just "..."
        assert!(
            redacted.len() <= 5,
            "total preview bytes {} must not exceed max_len",
            redacted.len()
        );
        assert!(std::str::from_utf8(redacted.as_bytes()).is_ok());
    }

    #[test]
    fn redact_and_truncate_total_never_exceeds_budget() {
        // ASCII text: each char is 1 byte, so budget calculation is straightforward
        let text = "a".repeat(100);
        let redacted = redact_and_truncate(&text, 20);
        assert!(
            redacted.len() <= 20,
            "total bytes {} must not exceed max_len=20",
            redacted.len()
        );
        assert!(redacted.ends_with("..."));
        assert_eq!(redacted.len(), 20); // 17 chars + "..."
    }

    #[test]
    fn cass_agent_display() {
        assert_eq!(CassAgent::Codex.as_str(), "codex");
        assert_eq!(CassAgent::ClaudeCode.as_str(), "claude_code");
        assert_eq!(format!("{}", CassAgent::Gemini), "gemini");
    }

    #[test]
    fn cass_agent_bridge_from_provider() {
        assert_eq!(
            CassAgent::from_provider(&AgentProvider::Claude),
            Some(CassAgent::ClaudeCode)
        );
        assert_eq!(
            CassAgent::from_provider(&AgentProvider::Codex),
            Some(CassAgent::Codex)
        );
        assert_eq!(
            CassAgent::from_provider(&AgentProvider::Gemini),
            Some(CassAgent::Gemini)
        );
        assert_eq!(
            CassAgent::from_provider(&AgentProvider::Unknown("chatgpt".to_string())),
            Some(CassAgent::ChatGpt)
        );
        assert_eq!(
            CassAgent::from_provider(&AgentProvider::GithubCopilot),
            None
        );
    }

    #[test]
    fn cass_agent_bridge_to_provider() {
        assert_eq!(CassAgent::ClaudeCode.to_provider(), AgentProvider::Claude);
        assert_eq!(CassAgent::Codex.to_provider(), AgentProvider::Codex);
        assert_eq!(
            CassAgent::ChatGpt.to_provider(),
            AgentProvider::Unknown("chatgpt".to_string())
        );
    }

    #[test]
    fn cass_agent_from_slug_uses_agent_provider_mapping() {
        assert_eq!(
            CassAgent::from_slug("claude-code"),
            Some(CassAgent::ClaudeCode)
        );
        assert_eq!(CassAgent::from_slug("codex"), Some(CassAgent::Codex));
        assert_eq!(CassAgent::from_slug("chat-gpt"), Some(CassAgent::ChatGpt));
        assert_eq!(CassAgent::from_slug("github-copilot"), None);
    }

    #[test]
    fn cass_error_remediation_not_installed() {
        let err = CassError::NotInstalled;
        let rem = err.remediation();
        assert!(!rem.summary.is_empty());
        assert!(!rem.commands.is_empty());
    }

    #[test]
    fn cass_error_remediation_timeout() {
        let err = CassError::Timeout { timeout_secs: 15 };
        let rem = err.remediation();
        assert!(rem.summary.contains("15s"));
    }

    #[test]
    fn cass_error_remediation_no_results() {
        let err = CassError::NoResults {
            query: "nonexistent".to_string(),
        };
        let rem = err.remediation();
        assert!(rem.summary.contains("nonexistent"));
    }

    #[test]
    fn categorize_not_found_as_not_installed() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let cass_err = categorize_io_error(&io_err);
        assert!(matches!(cass_err, CassError::NotInstalled));
    }

    #[test]
    fn categorize_other_io_as_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let cass_err = categorize_io_error(&io_err);
        match cass_err {
            CassError::Io { message } => assert!(message.contains("denied")),
            other => panic!("Expected Io, got: {other:?}"),
        }
    }

    #[test]
    fn search_options_default() {
        let opts = SearchOptions::default();
        assert!(opts.limit.is_none());
        assert!(opts.agent.is_none());
    }

    #[test]
    fn view_options_default() {
        let opts = ViewOptions::default();
        assert!(opts.context_lines.is_none());
    }

    #[test]
    fn summarize_session_with_roles() {
        let session = CassSession {
            started_at: Some("2026-01-29T09:58:00Z".to_string()),
            ended_at: Some("2026-01-29T10:02:00Z".to_string()),
            messages: vec![
                CassMessage {
                    role: Some("user".to_string()),
                    token_count: Some(10),
                    timestamp: Some("2026-01-29T10:00:00Z".to_string()),
                    ..CassMessage::default()
                },
                CassMessage {
                    role: Some("assistant".to_string()),
                    token_count: Some(20),
                    timestamp: Some("2026-01-29T10:01:00Z".to_string()),
                    ..CassMessage::default()
                },
                CassMessage {
                    role: Some("system".to_string()),
                    token_count: Some(5),
                    timestamp: Some("2026-01-29T09:59:00Z".to_string()),
                    ..CassMessage::default()
                },
            ],
            ..CassSession::default()
        };

        let summary = CassSessionSummary::from_session(&session);
        assert_eq!(summary.total_tokens, Some(35));
        assert_eq!(summary.input_tokens, Some(15));
        assert_eq!(summary.output_tokens, Some(20));
        assert_eq!(summary.message_count, 3);
        assert!(summary.session_started_at_ms.is_some());
        assert!(summary.session_ended_at_ms.is_some());
        assert!(summary.first_message_at_ms.is_some());
        assert!(summary.last_message_at_ms.is_some());
    }

    #[test]
    fn summarize_session_without_token_counts() {
        let session = CassSession {
            messages: vec![
                CassMessage {
                    role: Some("user".to_string()),
                    timestamp: Some("2026-01-29T10:00:00Z".to_string()),
                    ..CassMessage::default()
                },
                CassMessage {
                    role: Some("assistant".to_string()),
                    timestamp: Some("2026-01-29T10:01:00Z".to_string()),
                    ..CassMessage::default()
                },
            ],
            ..CassSession::default()
        };

        let summary = CassSessionSummary::from_session(&session);
        assert_eq!(summary.total_tokens, None);
        assert_eq!(summary.input_tokens, None);
        assert_eq!(summary.output_tokens, None);
        assert_eq!(summary.message_count, 2);
    }

    // ----------------------------------------------------------------
    // Expanded pure unit tests (wa-1u90p.7.1)
    // ----------------------------------------------------------------

    #[test]
    fn cass_agent_as_str_all_variants() {
        assert_eq!(CassAgent::Codex.as_str(), "codex");
        assert_eq!(CassAgent::ClaudeCode.as_str(), "claude_code");
        assert_eq!(CassAgent::Gemini.as_str(), "gemini");
        assert_eq!(CassAgent::Cursor.as_str(), "cursor");
        assert_eq!(CassAgent::Aider.as_str(), "aider");
        assert_eq!(CassAgent::ChatGpt.as_str(), "chatgpt");
    }

    #[test]
    fn cass_agent_display_matches_as_str() {
        let variants = [
            CassAgent::Codex,
            CassAgent::ClaudeCode,
            CassAgent::Gemini,
            CassAgent::Cursor,
            CassAgent::Aider,
            CassAgent::ChatGpt,
        ];
        for v in variants {
            assert_eq!(v.to_string(), v.as_str());
        }
    }

    #[test]
    fn cass_agent_clone_and_eq() {
        let a = CassAgent::Codex;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(CassAgent::Codex, CassAgent::Gemini);
    }

    #[test]
    fn cass_agent_debug() {
        let dbg = format!("{:?}", CassAgent::ClaudeCode);
        assert!(dbg.contains("ClaudeCode"));
    }

    #[test]
    fn cass_search_result_default() {
        let r = CassSearchResult::default();
        assert!(r.query.is_none());
        assert!(r.hits.is_empty());
        assert!(r.total_matches.is_none());
        assert!(r.extra.is_empty());
    }

    #[test]
    fn cass_search_result_serde_roundtrip() {
        let r = CassSearchResult {
            query: Some("error".to_string()),
            limit: Some(10),
            count: Some(2),
            total_matches: Some(5),
            hits: vec![CassSearchHit {
                agent: Some("s1".to_string()),
                score: Some(0.95),
                ..CassSearchHit::default()
            }],
            ..CassSearchResult::default()
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: CassSearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.query.as_deref(), Some("error"));
        assert_eq!(parsed.hits.len(), 1);
    }

    #[test]
    fn cass_search_hit_default() {
        let h = CassSearchHit::default();
        assert!(h.source_path.is_none());
        assert!(h.score.is_none());
    }

    #[test]
    fn cass_session_default() {
        let s = CassSession::default();
        assert!(s.session_id.is_none());
        assert!(s.messages.is_empty());
        assert!(s.extra.is_empty());
    }

    #[test]
    fn cass_message_default() {
        let m = CassMessage::default();
        assert!(m.role.is_none());
        assert!(m.content.is_none());
        assert!(m.token_count.is_none());
    }

    #[test]
    fn cass_view_result_default() {
        let v = CassViewResult::default();
        assert!(v.source_path.is_none());
        assert!(v.line_number.is_none());
        assert!(v.context_before.is_none());
    }

    #[test]
    fn cass_context_line_default() {
        let c = CassContextLine::default();
        assert!(c.line_number.is_none());
        assert!(c.content.is_none());
        assert!(c.role.is_none());
    }

    #[test]
    fn cass_status_default() {
        let s = CassStatus::default();
        assert!(s.healthy.is_none());
        assert!(s.total_sessions.is_none());
        assert!(s.stale.is_none());
    }

    #[test]
    fn cass_error_display_not_installed() {
        let err = CassError::NotInstalled;
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn cass_error_display_timeout() {
        let err = CassError::Timeout { timeout_secs: 30 };
        assert!(err.to_string().contains("30s"));
    }

    #[test]
    fn cass_error_display_non_zero_exit() {
        let err = CassError::NonZeroExit {
            status: 1,
            stderr: "error".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("exit code 1"));
        assert!(msg.contains("error"));
    }

    #[test]
    fn cass_error_display_output_too_large() {
        let err = CassError::OutputTooLarge {
            bytes: 2_000_000,
            max_bytes: 1_000_000,
        };
        assert!(err.to_string().contains("1000000"));
    }

    #[test]
    fn cass_error_display_invalid_json() {
        let err = CassError::InvalidJson {
            message: "bad".to_string(),
            preview: "...".to_string(),
        };
        assert!(err.to_string().contains("invalid JSON"));
    }

    #[test]
    fn cass_error_display_no_results() {
        let err = CassError::NoResults {
            query: "test".to_string(),
        };
        assert!(err.to_string().contains("no results"));
    }

    #[test]
    fn cass_error_display_io() {
        let err = CassError::Io {
            message: "broken pipe".to_string(),
        };
        assert!(err.to_string().contains("broken pipe"));
    }

    #[test]
    fn parse_cass_timestamp_ms_epoch_seconds() {
        // Smaller than 10 billion → treated as seconds
        let ms = parse_cass_timestamp_ms("1700000000").unwrap();
        assert_eq!(ms, 1_700_000_000_000);
    }

    #[test]
    fn parse_cass_timestamp_ms_epoch_millis() {
        // Larger than 10 billion → treated as milliseconds
        let ms = parse_cass_timestamp_ms("1700000000000").unwrap();
        assert_eq!(ms, 1_700_000_000_000);
    }

    #[test]
    fn parse_cass_timestamp_ms_rfc3339() {
        let ms = parse_cass_timestamp_ms("2026-01-15T12:00:00Z").unwrap();
        assert!(ms > 0);
    }

    #[test]
    fn parse_cass_timestamp_ms_empty() {
        assert!(parse_cass_timestamp_ms("").is_none());
    }

    #[test]
    fn parse_cass_timestamp_ms_whitespace() {
        assert!(parse_cass_timestamp_ms("   ").is_none());
    }

    #[test]
    fn parse_cass_timestamp_ms_garbage() {
        assert!(parse_cass_timestamp_ms("not-a-timestamp").is_none());
    }

    #[cfg(feature = "cass-export")]
    #[test]
    fn export_session_identifier_falls_back_to_row_id() {
        let mut session = AgentSessionRecord::new_start(7, "codex");
        session.id = 42;
        assert_eq!(export_session_identifier(&session), "ft-session-42");
    }

    #[cfg(feature = "cass-export")]
    #[test]
    fn export_session_identifier_preserves_explicit_session_id() {
        let mut session = AgentSessionRecord::new_start(7, "codex");
        session.id = 42;
        session.session_id = Some("sess-123".to_string());
        assert_eq!(export_session_identifier(&session), "sess-123");
    }

    #[cfg(feature = "cass-export")]
    #[test]
    fn estimate_text_tokens_uses_whitespace_and_char_fallback() {
        assert_eq!(estimate_text_tokens("alpha beta gamma"), 3);
        assert_eq!(estimate_text_tokens("  alpha   beta  "), 2);
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("abcdef"), 2);
    }

    #[test]
    fn classify_role_input_variants() {
        assert_eq!(classify_role("user"), RoleClass::Input);
        assert_eq!(classify_role("human"), RoleClass::Input);
        assert_eq!(classify_role("system"), RoleClass::Input);
        assert_eq!(classify_role("USER"), RoleClass::Input);
    }

    #[test]
    fn classify_role_output_variants() {
        assert_eq!(classify_role("assistant"), RoleClass::Output);
        assert_eq!(classify_role("model"), RoleClass::Output);
        assert_eq!(classify_role("ai"), RoleClass::Output);
        assert_eq!(classify_role("ASSISTANT"), RoleClass::Output);
    }

    #[test]
    fn classify_role_unknown() {
        assert_eq!(classify_role("tool"), RoleClass::Unknown);
        assert_eq!(classify_role(""), RoleClass::Unknown);
        assert_eq!(classify_role("moderator"), RoleClass::Unknown);
    }

    #[test]
    fn summarize_session_empty_messages() {
        let session = CassSession::default();
        let summary = CassSessionSummary::from_session(&session);
        assert_eq!(summary.message_count, 0);
        assert_eq!(summary.total_tokens, None);
        assert!(summary.first_message_at_ms.is_none());
        assert!(summary.last_message_at_ms.is_none());
    }

    // -------------------------------------------------------------------------
    // ft-xbnl0.2.3 Cx-first slice: tests for extracted helpers.
    // -------------------------------------------------------------------------

    #[test]
    fn build_search_args_minimal_options() {
        let options = SearchOptions::default();
        let args = build_search_args("hello world", &options);
        assert_eq!(
            args,
            vec![
                "search".to_string(),
                "hello world".to_string(),
                "--robot".to_string(),
            ]
        );
    }

    #[test]
    fn build_search_args_all_options_populated() {
        let options = SearchOptions {
            limit: Some(10),
            offset: Some(5),
            agent: Some(CassAgent::ClaudeCode),
            workspace: Some("/tmp/ws".to_string()),
            days: Some(30),
            fields: Some("minimal".to_string()),
            max_tokens: Some(4000),
        };
        let args = build_search_args("query", &options);
        // Validate order and payload of the argv tail (after --robot).
        assert!(args.starts_with(&[
            "search".to_string(),
            "query".to_string(),
            "--robot".to_string(),
        ]));
        assert!(args.iter().any(|a| a == "--limit"));
        assert!(args.iter().any(|a| a == "10"));
        assert!(args.iter().any(|a| a == "--offset"));
        assert!(args.iter().any(|a| a == "5"));
        assert!(args.iter().any(|a| a == "--agent"));
        assert!(args.iter().any(|a| a == "claude_code"));
        assert!(args.iter().any(|a| a == "--workspace"));
        assert!(args.iter().any(|a| a == "/tmp/ws"));
        assert!(args.iter().any(|a| a == "--days"));
        assert!(args.iter().any(|a| a == "30"));
        assert!(args.iter().any(|a| a == "--fields"));
        assert!(args.iter().any(|a| a == "minimal"));
        assert!(args.iter().any(|a| a == "--max-tokens"));
        assert!(args.iter().any(|a| a == "4000"));
    }

    #[test]
    fn build_search_args_with_only_limit() {
        let options = SearchOptions {
            limit: Some(50),
            ..SearchOptions::default()
        };
        let args = build_search_args("q", &options);
        assert_eq!(
            args,
            vec![
                "search".to_string(),
                "q".to_string(),
                "--robot".to_string(),
                "--limit".to_string(),
                "50".to_string(),
            ]
        );
    }

    #[test]
    fn build_search_sessions_args_without_agent() {
        let args = build_search_sessions_args(Path::new("/tmp/workspace"), None);
        assert_eq!(
            args,
            vec![
                "search".to_string(),
                "--path".to_string(),
                "/tmp/workspace".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ]
        );
    }

    #[test]
    fn build_search_sessions_args_with_agent() {
        let args = build_search_sessions_args(Path::new("/ws"), Some(CassAgent::Codex));
        assert_eq!(
            args.last().map(String::as_str),
            Some("codex"),
            "last arg must be the codex agent slug"
        );
        assert!(args.iter().any(|a| a == "--agent"));
        assert!(args.iter().any(|a| a == "--path"));
    }

    #[test]
    fn build_query_session_args_shape() {
        let args = build_query_session_args("session-abc-123");
        assert_eq!(
            args,
            vec![
                "query".to_string(),
                "--session-id".to_string(),
                "session-abc-123".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ]
        );
    }

    #[test]
    fn build_query_args_minimal() {
        let opts = ViewOptions::default();
        let args = build_query_args(Path::new("/tmp/s.jsonl"), 42, &opts);
        assert_eq!(
            args,
            vec![
                "view".to_string(),
                "/tmp/s.jsonl".to_string(),
                "-n".to_string(),
                "42".to_string(),
                "--json".to_string(),
            ]
        );
    }

    #[test]
    fn build_query_args_with_context() {
        let opts = ViewOptions {
            context_lines: Some(3),
        };
        let args = build_query_args(Path::new("/tmp/s.jsonl"), 10, &opts);
        assert!(args.ends_with(&["-C".to_string(), "3".to_string()]));
    }

    #[test]
    fn build_trigger_index_args_without_workspace() {
        let args = build_trigger_index_args(None);
        assert_eq!(args, vec!["index".to_string(), "--json".to_string()]);
    }

    #[test]
    fn build_trigger_index_args_with_workspace() {
        let args = build_trigger_index_args(Some("/custom/path"));
        assert_eq!(
            args,
            vec![
                "index".to_string(),
                "--json".to_string(),
                "--path".to_string(),
                "/custom/path".to_string(),
            ]
        );
    }

    /// ft-xbnl0.2.3 Cx-first: search_with_cx with an invalid binary path
    /// must surface an IO-shaped error (not Timeout, not NonZeroExit).
    /// Pins the error-surface contract so a future refactor doesn't
    /// accidentally flip invalid-binary into one of the other error
    /// surfaces.
    #[test]
    fn search_with_cx_returns_io_error_for_invalid_binary() {
        use crate::runtime_compat::CompatRuntime;

        let client = CassClient::new().with_binary("/nonexistent/cass-binary-path");
        let options = SearchOptions::default();

        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let result: Result<CassSearchResult, CassError> = runtime.block_on(async {
            let cx = crate::cx::for_request();
            client.search_with_cx(&cx, "test", &options).await
        });

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));

        // Invalid binary must surface an IO-shaped error. The exact
        // variant (NotFound/SpawnFailure) depends on how the OS
        // reports the failing exec, but it must NOT be Timeout or
        // NonZeroExit.
        match result {
            Err(err) => {
                let msg = err.to_string();
                assert!(
                    !msg.contains("timed out") && !msg.contains("non-zero"),
                    "invalid-binary search_with_cx should surface IO error, got: {msg}"
                );
            }
            Ok(_) => panic!("search_with_cx must fail for a nonexistent binary"),
        }
    }

    /// ft-xbnl0.2.3 Cx-first: tick 151 pins that
    /// `export_content_with_cx` routes through the new
    /// `resolve_export_session_with_cx` helper. On an empty DB the
    /// session lookup must return `Storage(NotFound)` from *both*
    /// the legacy and cx-first paths — byte-parity on the error type.
    #[cfg(feature = "cass-export")]
    #[test]
    fn cass_export_content_with_cx_missing_session_matches_legacy() {
        use crate::runtime_compat::CompatRuntime;

        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let tmp = std::env::temp_dir().join(format!(
                    "wa_test_cass_export_content_cx_{}.db",
                    std::process::id()
                ));
                let db_path = tmp.to_string_lossy().to_string();
                let storage = StorageHandle::new(&db_path).await.unwrap();

                let query = CassContentExportQuery {
                    after_id: None,
                    limit: 100,
                };

                let cx = crate::cx::for_request();
                let legacy_err = export_content(&storage, "session-tick151-none", &query)
                    .await
                    .expect_err("missing session must surface an error");
                let cx_err = export_content_with_cx(&cx, &storage, "session-tick151-none", &query)
                    .await
                    .expect_err("missing session must surface an error via cx-first path");

                match (&legacy_err, &cx_err) {
                    (
                        crate::Error::Storage(crate::StorageError::NotFound(a)),
                        crate::Error::Storage(crate::StorageError::NotFound(b)),
                    ) => {
                        assert_eq!(a, b, "NotFound messages should match on both paths");
                    }
                    other => panic!("expected Storage(NotFound) on both paths, got {other:?}"),
                }

                storage.shutdown().await.unwrap();
                let _ = std::fs::remove_file(&tmp);
            });
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// ft-xbnl0.2.3 Cx-first: `export_sessions_with_cx` with a
    /// fresh cx on an empty DB must return an empty Vec (same
    /// as the legacy `export_sessions` on an empty DB).
    #[cfg(feature = "cass-export")]
    #[test]
    fn cass_export_sessions_with_cx_empty_db_matches_legacy() {
        use crate::runtime_compat::CompatRuntime;

        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let tmp = std::env::temp_dir().join(format!(
                    "wa_test_cass_export_sessions_cx_{}.db",
                    std::process::id()
                ));
                let db_path = tmp.to_string_lossy().to_string();
                let storage = StorageHandle::new(&db_path).await.unwrap();

                let query = CassExportQuery {
                    pane_id: None,
                    since: None,
                    until: None,
                    after_id: None,
                    limit: 10,
                };

                let cx = crate::cx::for_request();
                let legacy = export_sessions(&storage, &query).await.unwrap();
                let cx_first = export_sessions_with_cx(&cx, &storage, &query)
                    .await
                    .unwrap();

                assert_eq!(legacy.len(), cx_first.len());
                assert_eq!(legacy.len(), 0, "empty DB should return 0 sessions");

                storage.shutdown().await.unwrap();
                let _ = std::fs::remove_file(&tmp);
            });
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
