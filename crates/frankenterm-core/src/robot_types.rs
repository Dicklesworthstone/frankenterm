//! Typed client types for ft robot/MCP JSON responses.
//!
//! These types mirror the serialization-side types in the `wa` binary and provide
//! `Deserialize` so consumers can parse robot JSON output without hand-parsing.
//!
//! # Usage
//!
//! ```no_run
//! use frankenterm_core::robot_types::{RobotResponse, GetTextData};
//!
//! let json = r#"{"ok":true,"data":{"pane_id":1,"text":"hello","tail_lines":100,"escapes_included":false},"elapsed_ms":5,"version":"0.1.0","now":1700000000000}"#;
//! let resp: RobotResponse<GetTextData> = serde_json::from_str(json).unwrap();
//! assert!(resp.ok);
//! assert_eq!(resp.data.unwrap().text, "hello");
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error_codes::ErrorCategory;

// ============================================================================
// Envelope
// ============================================================================

/// The standard JSON envelope wrapping all robot mode responses.
///
/// Every `ft robot <command> --format json` call returns this envelope.
/// Use `parse_response` or `RobotResponse::<T>::from_json` for convenience.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(deserialize = "T: serde::de::DeserializeOwned"))]
pub struct RobotResponse<T> {
    /// `true` when the command succeeded.
    pub ok: bool,
    /// Command-specific payload (present when `ok == true`).
    #[serde(default)]
    pub data: Option<T>,
    /// Human-readable error message (present when `ok == false`).
    #[serde(default)]
    pub error: Option<String>,
    /// Machine-readable error code like `"FT-1001"` (present when `ok == false`).
    #[serde(default)]
    pub error_code: Option<String>,
    /// Actionable hint for recovery (present on some errors).
    #[serde(default)]
    pub hint: Option<String>,
    /// Wall-clock milliseconds the command took.
    pub elapsed_ms: u64,
    /// ft version that produced this response.
    pub version: String,
    /// Unix epoch milliseconds when the response was generated.
    pub now: u64,
}

impl<T> RobotResponse<T> {
    /// Build a success envelope wrapping `data`.
    pub fn success(data: T, elapsed_ms: u64) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            hint: None,
            elapsed_ms,
            version: crate::VERSION.to_string(),
            now,
        }
    }
}

impl<T: serde::de::DeserializeOwned> RobotResponse<T> {
    /// Parse a JSON string into a typed response.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Parse a JSON byte slice into a typed response.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Returns the data if `ok == true`, otherwise returns an error with the
    /// error message and code from the response.
    pub fn into_result(self) -> Result<T, RobotError> {
        if self.ok {
            match self.data {
                Some(data) => Ok(data),
                None => Err(RobotError {
                    code: self.error_code,
                    message: "ok=true but data is null".to_string(),
                    hint: None,
                }),
            }
        } else {
            Err(RobotError {
                code: self.error_code,
                message: self.error.unwrap_or_else(|| "unknown error".to_string()),
                hint: self.hint,
            })
        }
    }

    /// Returns the parsed `ErrorCode` if present.
    pub fn parsed_error_code(&self) -> Option<ErrorCode> {
        self.error_code.as_deref().and_then(ErrorCode::parse)
    }
}

/// Error extracted from a failed `RobotResponse`.
#[derive(Debug, Clone)]
pub struct RobotError {
    /// Machine-readable error code (e.g. `"FT-1001"`).
    pub code: Option<String>,
    /// Human-readable error message.
    pub message: String,
    /// Actionable hint for recovery.
    pub hint: Option<String>,
}

impl std::fmt::Display for RobotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = &self.code {
            write!(f, "[{}] {}", code, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for RobotError {}

// ============================================================================
// Error codes
// ============================================================================

/// Parsed error code from ft robot responses.
///
/// Maps the `FT-xxxx` string codes from `error_codes.rs` into a structured enum
/// for pattern matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    // WezTerm (1xxx)
    /// WA-1001: WezTerm CLI not found
    WeztermNotFound,
    /// WA-1002: WezTerm CLI execution failed
    WeztermExecFailed,
    /// WA-1003: WezTerm pane not found
    PaneNotFound,
    /// WA-1004: WezTerm output parse error
    WeztermParseFailed,
    /// WA-1005: WezTerm connection refused
    WeztermConnectionRefused,

    // Storage (2xxx)
    /// WA-2001: Database locked
    DatabaseLocked,
    /// WA-2002: Storage corruption detected
    StorageCorruption,
    /// WA-2003: FTS5 index error
    FtsIndexError,
    /// WA-2004: Migration failed
    MigrationFailed,
    /// WA-2005: Disk full
    DiskFull,

    // Pattern (3xxx)
    /// WA-3001: Invalid regex pattern
    InvalidRegex,
    /// WA-3002: Rule pack not found
    RulePackNotFound,
    /// WA-3003: Pattern match timeout
    PatternTimeout,

    // Policy (4xxx)
    /// WA-4001: Action denied by policy
    ActionDenied,
    /// WA-4002: Rate limit exceeded
    RateLimitExceeded,
    /// WA-4003: Approval required
    ApprovalRequired,
    /// WA-4004: Approval expired
    ApprovalExpired,

    // Workflow (5xxx)
    /// WA-5001: Workflow not found
    WorkflowNotFound,
    /// WA-5002: Workflow step failed
    WorkflowStepFailed,
    /// WA-5003: Workflow timeout
    WorkflowTimeout,
    /// WA-5004: Workflow already running
    WorkflowAlreadyRunning,

    // Network (6xxx)
    /// WA-6001: Network timeout
    NetworkTimeout,
    /// WA-6002: Connection refused
    ConnectionRefused,

    // Config (7xxx)
    /// WA-7001: Config file invalid
    ConfigInvalid,
    /// WA-7002: Config file not found
    ConfigNotFound,

    // Internal (9xxx)
    /// WA-9001: Internal error
    InternalError,
    /// WA-9002: Feature not available
    FeatureNotAvailable,
    /// WA-9003: Version mismatch
    VersionMismatch,

    /// Unknown code not in the catalog.
    Unknown(u16),
}

impl ErrorCode {
    /// Parse a `"FT-xxxx"` string into an `ErrorCode`.
    ///
    /// Returns `None` if the string doesn't match the `WA-` prefix.
    pub fn parse(s: &str) -> Option<Self> {
        let num_str = s.strip_prefix("FT-")?;
        let num: u16 = num_str.parse().ok()?;
        Some(Self::from_number(num))
    }

    /// Map a numeric code to the variant.
    pub fn from_number(n: u16) -> Self {
        match n {
            1001 => Self::WeztermNotFound,
            1002 => Self::WeztermExecFailed,
            1003 => Self::PaneNotFound,
            1004 => Self::WeztermParseFailed,
            1005 => Self::WeztermConnectionRefused,
            2001 => Self::DatabaseLocked,
            2002 => Self::StorageCorruption,
            2003 => Self::FtsIndexError,
            2004 => Self::MigrationFailed,
            2005 => Self::DiskFull,
            3001 => Self::InvalidRegex,
            3002 => Self::RulePackNotFound,
            3003 => Self::PatternTimeout,
            4001 => Self::ActionDenied,
            4002 => Self::RateLimitExceeded,
            4003 => Self::ApprovalRequired,
            4004 => Self::ApprovalExpired,
            5001 => Self::WorkflowNotFound,
            5002 => Self::WorkflowStepFailed,
            5003 => Self::WorkflowTimeout,
            5004 => Self::WorkflowAlreadyRunning,
            6001 => Self::NetworkTimeout,
            6002 => Self::ConnectionRefused,
            7001 => Self::ConfigInvalid,
            7002 => Self::ConfigNotFound,
            9001 => Self::InternalError,
            9002 => Self::FeatureNotAvailable,
            9003 => Self::VersionMismatch,
            other => Self::Unknown(other),
        }
    }

    /// Returns the `"FT-xxxx"` string form.
    pub fn as_str(&self) -> String {
        format!("FT-{}", self.number())
    }

    /// Returns the numeric part of the code.
    pub fn number(&self) -> u16 {
        match self {
            Self::WeztermNotFound => 1001,
            Self::WeztermExecFailed => 1002,
            Self::PaneNotFound => 1003,
            Self::WeztermParseFailed => 1004,
            Self::WeztermConnectionRefused => 1005,
            Self::DatabaseLocked => 2001,
            Self::StorageCorruption => 2002,
            Self::FtsIndexError => 2003,
            Self::MigrationFailed => 2004,
            Self::DiskFull => 2005,
            Self::InvalidRegex => 3001,
            Self::RulePackNotFound => 3002,
            Self::PatternTimeout => 3003,
            Self::ActionDenied => 4001,
            Self::RateLimitExceeded => 4002,
            Self::ApprovalRequired => 4003,
            Self::ApprovalExpired => 4004,
            Self::WorkflowNotFound => 5001,
            Self::WorkflowStepFailed => 5002,
            Self::WorkflowTimeout => 5003,
            Self::WorkflowAlreadyRunning => 5004,
            Self::NetworkTimeout => 6001,
            Self::ConnectionRefused => 6002,
            Self::ConfigInvalid => 7001,
            Self::ConfigNotFound => 7002,
            Self::InternalError => 9001,
            Self::FeatureNotAvailable => 9002,
            Self::VersionMismatch => 9003,
            Self::Unknown(n) => *n,
        }
    }

    /// Returns the error category.
    pub fn category(&self) -> ErrorCategory {
        match self.number() / 1000 {
            1 => ErrorCategory::Wezterm,
            2 => ErrorCategory::Storage,
            3 => ErrorCategory::Pattern,
            4 => ErrorCategory::Policy,
            5 => ErrorCategory::Workflow,
            6 => ErrorCategory::Network,
            7 => ErrorCategory::Config,
            _ => ErrorCategory::Internal,
        }
    }

    /// Returns `true` if this is a retryable error.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::DatabaseLocked
                | Self::RateLimitExceeded
                | Self::NetworkTimeout
                | Self::ConnectionRefused
                | Self::PatternTimeout
                | Self::WeztermConnectionRefused
        )
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// Pane operations
// ============================================================================

/// Response data for `ft robot get-text`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetTextData {
    pub pane_id: u64,
    pub text: String,
    pub tail_lines: usize,
    pub escapes_included: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub truncation_info: Option<TruncationInfo>,
}

/// Response data for batched `ft robot get-text --panes ...` and `--all`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchGetTextData {
    pub pane_ids: Vec<u64>,
    pub tail_lines: usize,
    pub escapes_included: bool,
    pub results: BTreeMap<u64, PaneTextResult>,
}

/// Response data for `ft robot state --include-text`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateWithTextData {
    pub panes: Vec<PaneStateData>,
    pub tail_lines: usize,
    pub escapes_included: bool,
    pub pane_text: BTreeMap<u64, PaneTextResult>,
}

/// Pane metadata entry returned by `ft robot state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneStateData {
    pub pane_id: u64,
    #[serde(default)]
    pub pane_uuid: Option<String>,
    pub tab_id: u64,
    pub window_id: u64,
    pub domain: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub observed: bool,
    #[serde(default)]
    pub ignore_reason: Option<String>,
}

/// Per-pane result entry for batch pane text reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PaneTextResult {
    Ok {
        text: String,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        truncation_info: Option<TruncationInfo>,
    },
    Error {
        code: String,
        message: String,
        #[serde(default)]
        hint: Option<String>,
    },
}

/// Truncation details when pane output exceeds limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TruncationInfo {
    pub original_bytes: usize,
    pub returned_bytes: usize,
    pub original_lines: usize,
    pub returned_lines: usize,
}

/// Response data for `ft robot send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendData {
    pub pane_id: u64,
    pub injection: serde_json::Value,
    #[serde(default)]
    pub wait_for: Option<WaitForData>,
    #[serde(default)]
    pub verification_error: Option<String>,
}

/// Wait-for result data (used by send and wait-for commands).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitForData {
    pub pane_id: u64,
    pub pattern: String,
    pub matched: bool,
    pub elapsed_ms: u64,
    pub polls: usize,
    #[serde(default)]
    pub is_regex: bool,
}

// ============================================================================
// Search & Events
// ============================================================================

/// Response data for `ft robot search`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchData {
    pub query: String,
    pub results: Vec<SearchHit>,
    pub total_hits: usize,
    pub limit: usize,
    #[serde(default)]
    pub pane_filter: Option<u64>,
    #[serde(default)]
    pub since_filter: Option<i64>,
    #[serde(default)]
    pub until_filter: Option<i64>,
    /// Search mode used (fts5, lexical, semantic, hybrid, two-tier).
    #[serde(default)]
    pub mode: Option<String>,
    /// Tier-level metrics from the search pipeline.
    #[serde(default)]
    pub metrics: Option<serde_json::Value>,
}

/// Individual search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub segment_id: i64,
    pub pane_id: u64,
    pub seq: u64,
    pub captured_at: i64,
    pub score: f64,
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    /// Semantic similarity score (when using semantic/hybrid mode).
    #[serde(default)]
    pub semantic_score: Option<f64>,
    /// Fused rank position from RRF (when using hybrid mode).
    #[serde(default)]
    pub fusion_rank: Option<usize>,
}

/// Response data for `ft robot search-index stats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchIndexStatsData {
    pub index_dir: String,
    pub state_path: String,
    pub format_version: u32,
    pub document_count: usize,
    pub segment_count: usize,
    pub index_size_bytes: u64,
    pub pending_docs: usize,
    pub max_index_size_bytes: u64,
    pub ttl_days: u64,
    pub flush_interval_secs: u64,
    pub flush_docs_threshold: usize,
    #[serde(default)]
    pub newest_captured_at_ms: Option<i64>,
    #[serde(default)]
    pub oldest_captured_at_ms: Option<i64>,
    #[serde(default)]
    pub freshness_age_ms: Option<i64>,
    #[serde(default)]
    pub last_update_ts: Option<i64>,
    #[serde(default)]
    pub source_counts: BTreeMap<String, usize>,
    #[serde(default)]
    pub embedder_tiers_available: Vec<String>,
    pub background_job_status: String,
    pub indexing_error_count: usize,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Per-result scoring breakdown for `ft robot search --explain`.
///
/// Provides full visibility into how each search result was scored across
/// the multi-tier pipeline: lexical (BM25), semantic similarity, RRF fusion,
/// and optional cross-encoder reranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchScoringBreakdown {
    /// BM25 lexical score (if lexical/hybrid mode).
    #[serde(default)]
    pub bm25_score: Option<f64>,
    /// Matching terms from the lexical query (highlighted).
    #[serde(default)]
    pub matching_terms: Vec<String>,
    /// Semantic cosine similarity score (if semantic/hybrid mode).
    #[serde(default)]
    pub semantic_similarity: Option<f64>,
    /// Embedder tier that produced the semantic vector.
    #[serde(default)]
    pub embedder_tier: Option<String>,
    /// Reciprocal Rank Fusion rank position (1-based).
    #[serde(default)]
    pub rrf_rank: Option<usize>,
    /// RRF fused score.
    #[serde(default)]
    pub rrf_score: Option<f64>,
    /// Cross-encoder reranker score (if quality tier completed).
    #[serde(default)]
    pub reranker_score: Option<f64>,
    /// Final combined score used for ordering.
    pub final_score: f64,
}

/// Extended search hit with per-result scoring explanation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainedSearchHit {
    /// Base search hit fields.
    #[serde(flatten)]
    pub hit: SearchHit,
    /// Detailed scoring breakdown.
    pub scoring: SearchScoringBreakdown,
}

/// Response data for `ft robot search --explain`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchExplainData {
    pub query: String,
    pub results: Vec<ExplainedSearchHit>,
    pub total_hits: usize,
    pub limit: usize,
    #[serde(default)]
    pub pane_filter: Option<u64>,
    /// Search mode used.
    pub mode: String,
    /// Pipeline timing breakdown.
    #[serde(default)]
    pub timing: Option<SearchPipelineTiming>,
    /// Two-tier metrics from the search pipeline.
    #[serde(default)]
    pub tier_metrics: Option<serde_json::Value>,
}

/// Timing breakdown for the search pipeline phases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPipelineTiming {
    /// Total query time in microseconds.
    pub total_us: u64,
    /// Lexical retrieval time in microseconds.
    #[serde(default)]
    pub lexical_us: Option<u64>,
    /// Semantic retrieval time in microseconds.
    #[serde(default)]
    pub semantic_us: Option<u64>,
    /// RRF fusion time in microseconds.
    #[serde(default)]
    pub fusion_us: Option<u64>,
    /// Reranking time in microseconds.
    #[serde(default)]
    pub rerank_us: Option<u64>,
}

/// Phase marker for streaming JSONL search output.
///
/// Emitted between result batches in `--format jsonl` to indicate
/// which tier of the search pipeline produced the following results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SearchStreamPhase {
    /// Fast tier results (hash embedder + BM25).
    #[serde(rename = "phase_fast")]
    Fast { result_count: usize },
    /// Quality tier results (model2vec/fastembed + cross-encoder).
    #[serde(rename = "phase_quality")]
    Quality { result_count: usize },
    /// Search complete.
    #[serde(rename = "phase_done")]
    Done {
        total_results: usize,
        total_us: u64,
    },
}

/// Indexing pipeline status for `ft robot search-index pipeline`.
///
/// Extends `SearchIndexStatsData` with pipeline-level operational state
/// from [`ContentIndexingPipeline`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPipelineStatusData {
    /// Pipeline lifecycle state (running, paused, stopped).
    pub state: String,
    /// Per-pane watermark info.
    pub watermarks: Vec<PipelineWatermarkInfo>,
    /// Total indexing ticks performed.
    pub total_ticks: u64,
    /// Total documents indexed across all panes.
    pub total_docs_indexed: u64,
    /// Total scrollback lines consumed across all panes.
    pub total_lines_consumed: u64,
    /// Index-level stats snapshot.
    #[serde(default)]
    pub index_stats: Option<serde_json::Value>,
}

/// Per-pane watermark info for pipeline status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineWatermarkInfo {
    /// Pane identifier.
    pub pane_id: u64,
    /// Highest captured_at_ms value that has been indexed.
    pub last_indexed_at_ms: i64,
    /// Total documents indexed from this pane.
    pub total_docs_indexed: u64,
    /// Session ID associated with this pane.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Response for `ft robot search-index pause/resume/rebuild` control commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPipelineControlResult {
    /// The action that was performed.
    pub action: String,
    /// Whether the action succeeded.
    pub success: bool,
    /// Pipeline state after the action.
    pub state_after: String,
    /// Optional message providing context.
    #[serde(default)]
    pub message: Option<String>,
}

/// Response data for `ft robot events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsData {
    pub events: Vec<EventItem>,
    pub total_count: usize,
    pub limit: usize,
    #[serde(default)]
    pub pane_filter: Option<u64>,
    #[serde(default)]
    pub rule_id_filter: Option<String>,
    #[serde(default)]
    pub event_type_filter: Option<String>,
    #[serde(default)]
    pub triage_state_filter: Option<String>,
    #[serde(default)]
    pub label_filter: Option<String>,
    #[serde(default)]
    pub unhandled_only: bool,
    #[serde(default)]
    pub since_filter: Option<i64>,
    #[serde(default)]
    pub would_handle: bool,
    #[serde(default)]
    pub dry_run: bool,
}

/// Individual event item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventItem {
    pub id: i64,
    pub pane_id: u64,
    pub rule_id: String,
    pub pack_id: String,
    pub event_type: String,
    pub severity: String,
    pub confidence: f64,
    #[serde(default)]
    pub extracted: Option<serde_json::Value>,
    #[serde(default)]
    pub annotations: Option<serde_json::Value>,
    pub captured_at: i64,
    #[serde(default)]
    pub handled_at: Option<i64>,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub would_handle_with: Option<EventWouldHandle>,
}

/// Workflow preview for events dry-run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventWouldHandle {
    pub workflow: String,
    #[serde(default)]
    pub preview_command: Option<String>,
    #[serde(default)]
    pub first_step: Option<String>,
    #[serde(default)]
    pub estimated_duration_ms: Option<u64>,
    #[serde(default)]
    pub would_run: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response data for event annotation/triage mutations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMutationData {
    pub event_id: i64,
    #[serde(default)]
    pub changed: Option<bool>,
    pub annotations: serde_json::Value,
}

// ============================================================================
// Agent Inventory
// ============================================================================

/// Response data for `ft robot agents list` / `ft robot agents inventory`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInventoryData {
    /// Agents detected via filesystem probes.
    pub installed: Vec<InstalledAgentInfo>,
    /// Agents currently active in panes, keyed by pane_id.
    pub running: std::collections::BTreeMap<u64, RunningAgentInfo>,
    /// Aggregate counts.
    pub summary: AgentInventorySummary,
    /// Whether the `agent-detection` feature is compiled in.
    pub filesystem_detection_available: bool,
}

/// Individual installed agent from filesystem detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledAgentInfo {
    /// Canonical agent slug (e.g. "claude", "codex", "gemini").
    pub slug: String,
    /// Human-readable display name (e.g. "Claude Code", "Codex").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether the agent was detected on this machine.
    pub detected: bool,
    /// Human-readable evidence strings from probes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Filesystem root paths that were found.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_paths: Vec<String>,
    /// Path to agent's configuration file, if found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// Path to agent binary, if found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// Detected agent version string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Agent currently active in a pane (from correlator).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningAgentInfo {
    /// Canonical agent slug.
    pub slug: String,
    /// Human-readable display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Inferred agent state: "starting", "working", "rate_limited",
    /// "waiting_approval", "idle", "active", "unknown".
    pub state: String,
    /// Session ID extracted from agent output, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// How this agent was detected.
    pub source: String,
    /// Pane ID where this agent is running.
    pub pane_id: u64,
}

/// Aggregate counts for agent inventory.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentInventorySummary {
    /// Number of distinct agents detected on filesystem.
    pub installed_count: usize,
    /// Number of panes with an active agent.
    pub running_count: usize,
    /// Number of installed agents that have configuration files.
    pub configured_count: usize,
    /// Number of installed agents not currently running.
    pub installed_but_idle_count: usize,
}

/// Response data for `ft robot agents detect --refresh`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetectRefreshResult {
    /// Whether the refresh was performed.
    pub refreshed: bool,
    /// Number of agents detected after refresh.
    pub detected_count: usize,
    /// Total agents probed.
    pub total_probed: usize,
    /// Optional message (e.g. "feature not available").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl From<&crate::agent_correlator::InstalledAgentInventoryEntry> for InstalledAgentInfo {
    fn from(entry: &crate::agent_correlator::InstalledAgentInventoryEntry) -> Self {
        Self {
            slug: entry.slug.clone(),
            display_name: None,
            detected: entry.detected,
            evidence: entry.evidence.clone(),
            root_paths: entry.root_paths.clone(),
            config_path: entry.config_path.clone(),
            binary_path: entry.binary_path.clone(),
            version: entry.version.clone(),
        }
    }
}

impl From<(u64, &crate::agent_correlator::RunningAgentInventoryEntry)> for RunningAgentInfo {
    fn from((pane_id, entry): (u64, &crate::agent_correlator::RunningAgentInventoryEntry)) -> Self {
        Self {
            slug: entry.slug.clone(),
            display_name: None,
            state: entry.state.clone(),
            session_id: entry.session_id.clone(),
            source: match entry.source {
                crate::agent_correlator::DetectionSource::PatternEngine => "pattern_engine",
                crate::agent_correlator::DetectionSource::PaneTitle => "pane_title",
                crate::agent_correlator::DetectionSource::ProcessName => "process_name",
            }
            .to_string(),
            pane_id,
        }
    }
}

impl From<&crate::agent_correlator::AgentInventory> for AgentInventoryData {
    fn from(inv: &crate::agent_correlator::AgentInventory) -> Self {
        let installed: Vec<InstalledAgentInfo> =
            inv.installed.iter().map(InstalledAgentInfo::from).collect();
        let running: std::collections::BTreeMap<u64, RunningAgentInfo> = inv
            .running
            .iter()
            .map(|(&pid, entry)| (pid, RunningAgentInfo::from((pid, entry))))
            .collect();

        let running_slugs: std::collections::HashSet<&str> =
            running.values().map(|r| r.slug.as_str()).collect();
        let installed_count = installed.iter().filter(|a| a.detected).count();
        let running_count = running.len();
        let configured_count = installed.iter().filter(|a| a.config_path.is_some()).count();
        let installed_but_idle_count = installed
            .iter()
            .filter(|a| a.detected && !running_slugs.contains(a.slug.as_str()))
            .count();

        Self {
            installed,
            running,
            summary: AgentInventorySummary {
                installed_count,
                running_count,
                configured_count,
                installed_but_idle_count,
            },
            filesystem_detection_available: crate::agent_correlator::filesystem_detection_available(),
        }
    }
}

// ============================================================================
// Agent Configuration (ft-dr6zv.2.4)
// ============================================================================

/// Response data for `ft robot agents configure`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigureData {
    /// Results per agent.
    pub results: Vec<AgentConfigureResultItem>,
    /// Total agents processed.
    pub total: usize,
    /// Number of files created.
    pub created: usize,
    /// Number of files updated (append or replace).
    pub updated: usize,
    /// Number of files skipped (already current).
    pub skipped: usize,
    /// Number of errors.
    pub errors: usize,
}

/// Per-agent result in a configure operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigureResultItem {
    /// Agent slug.
    pub slug: String,
    /// Human-readable agent name.
    pub display_name: String,
    /// What action was taken.
    pub action: String,
    /// Target file path (relative).
    pub filename: String,
    /// Whether a backup was created before modification.
    pub backup_created: bool,
    /// Error message if this agent's config generation failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response data for `ft robot agents configure --dry-run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigureDryRunData {
    /// Plan items showing what would happen.
    pub plan: Vec<AgentConfigurePlanItem>,
    /// Total agents in the plan.
    pub total: usize,
    /// How many would create new files.
    pub would_create: usize,
    /// How many would modify existing files.
    pub would_modify: usize,
    /// How many would be skipped.
    pub would_skip: usize,
}

/// A single item in a configure dry-run plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigurePlanItem {
    /// Agent slug.
    pub slug: String,
    /// Human-readable agent name.
    pub display_name: String,
    /// Config file type (e.g. "claude_md", "agents_md").
    pub config_kind: String,
    /// Where the file would be placed.
    pub scope: String,
    /// Target file path (relative).
    pub filename: String,
    /// Whether the target file already exists.
    pub file_exists: bool,
    /// Whether the FrankenTerm section already exists in the file.
    pub section_exists: bool,
    /// What action would be taken.
    pub action: String,
    /// Preview of the content that would be written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_preview: Option<String>,
}

// ============================================================================
// Mission (ft-1i2ge.5.2)
// ============================================================================

/// Run-state classification for a mission assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionRunState {
    Pending,
    Succeeded,
    Failed,
    Cancelled,
}

/// Approval-gate state classification for a mission assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionAgentState {
    NotRequired,
    Pending,
    Approved,
    Denied,
    Expired,
}

/// Composite action readiness for a mission assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionActionState {
    Ready,
    Blocked,
    Completed,
}

/// Filter parameters for mission state and decision queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionStateFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_state: Option<crate::plan::MissionLifecycleState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_state: Option<MissionRunState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_state: Option<MissionAgentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_state: Option<MissionActionState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    pub limit: usize,
}

/// Per-outcome counters across mission assignments.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MissionAssignmentCounters {
    pub pending_approval: usize,
    pub approved: usize,
    pub denied: usize,
    pub expired: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub unresolved: usize,
}

/// Available state transition from the current lifecycle position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionTransitionInfo {
    pub kind: String,
    pub to: String,
}

/// Individual assignment in the robot mission state payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionAssignmentData {
    pub assignment_id: String,
    pub candidate_id: String,
    pub assignee: String,
    pub assigned_by: crate::plan::MissionActorRole,
    pub action_type: String,
    pub run_state: MissionRunState,
    pub agent_state: MissionAgentState,
    pub action_state: MissionActionState,
    pub approval_state: crate::plan::ApprovalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<crate::plan::Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// Response data for `ft robot mission state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionStateData {
    pub mission_file: String,
    pub mission_id: String,
    pub title: String,
    pub mission_hash: String,
    pub lifecycle_state: crate::plan::MissionLifecycleState,
    pub mission_matches_filter: bool,
    pub candidate_count: usize,
    pub assignment_count: usize,
    pub matched_assignment_count: usize,
    pub returned_assignment_count: usize,
    pub filters: MissionStateFilters,
    pub assignment_counters: MissionAssignmentCounters,
    pub available_transitions: Vec<MissionTransitionInfo>,
    pub assignments: Vec<MissionAssignmentData>,
}

/// Failure classification entry for mission decision explainability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionFailureCatalogEntry {
    pub reason_code: String,
    pub error_code: String,
    pub terminality: String,
    pub retryability: String,
    pub human_hint: String,
    pub machine_hint: String,
}

/// Per-assignment decision data with dispatch details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDecisionData {
    pub assignment: MissionAssignmentData,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_action: Option<crate::plan::StepAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_contract: Option<crate::plan::MissionDispatchContract>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_target: Option<crate::plan::MissionDispatchTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run_execution: Option<crate::plan::MissionDispatchExecution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_error: Option<String>,
}

/// Response data for `ft robot mission decisions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDecisionsData {
    pub mission_file: String,
    pub mission_id: String,
    pub title: String,
    pub mission_hash: String,
    pub lifecycle_state: crate::plan::MissionLifecycleState,
    pub mission_matches_filter: bool,
    pub candidate_count: usize,
    pub assignment_count: usize,
    pub matched_assignment_count: usize,
    pub returned_assignment_count: usize,
    pub filters: MissionStateFilters,
    pub available_transitions: Vec<MissionTransitionInfo>,
    pub failure_catalog: Vec<MissionFailureCatalogEntry>,
    pub decisions: Vec<MissionDecisionData>,
}

// ============================================================================
// Transactional Execution (ft-1i2ge.8.8)
// ============================================================================

/// Risk classification for a transaction step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxStepRisk {
    Low,
    Medium,
    High,
    Critical,
}

/// Transaction lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxPhaseState {
    Planned,
    Preparing,
    Committing,
    Compensating,
    Completed,
    Aborted,
}

/// Precondition kind for a transaction step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TxPreconditionKind {
    PolicyApproved,
    ReservationHeld { paths: Vec<String> },
    ApprovalRequired { approver: String },
    TargetReachable { target_id: String },
    ContextFresh { max_age_ms: u64 },
}

/// Precondition on a transaction step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxPreconditionData {
    pub kind: TxPreconditionKind,
    pub description: String,
    pub required: bool,
}

/// Compensation kind for rollback/recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TxCompensationKind {
    Rollback,
    NotifyOperator,
    RetryWithBackoff { max_retries: u32 },
    SkipAndContinue,
    Alternative { alternative_step_id: String },
}

/// Compensating action associated with a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxCompensatingActionData {
    pub step_id: String,
    pub description: String,
    pub action_type: TxCompensationKind,
}

/// Risk summary across all steps in a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRiskSummaryData {
    pub total_steps: usize,
    pub high_risk_count: usize,
    pub critical_risk_count: usize,
    pub uncompensated_steps: usize,
    pub overall_risk: TxStepRisk,
}

/// Rejected dependency edge in plan compilation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRejectedEdgeData {
    pub from_step: String,
    pub to_step: String,
    pub reason: String,
}

/// Step data in a compiled transaction plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxStepData {
    pub id: String,
    pub bead_id: String,
    pub agent_id: String,
    pub description: String,
    pub depends_on: Vec<String>,
    pub preconditions: Vec<TxPreconditionData>,
    pub compensations: Vec<TxCompensatingActionData>,
    pub risk: TxStepRisk,
    pub score: f64,
}

/// Response data for `ft robot tx plan`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxPlanData {
    pub plan_id: String,
    pub plan_hash: u64,
    pub steps: Vec<TxStepData>,
    pub execution_order: Vec<String>,
    pub parallel_levels: Vec<Vec<String>>,
    pub risk_summary: TxRiskSummaryData,
    pub rejected_edges: Vec<TxRejectedEdgeData>,
}

/// Step outcome in a transaction execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TxStepOutcome {
    Success {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    Failed {
        error_code: String,
        error_message: String,
        compensated: bool,
    },
    Skipped {
        reason: String,
    },
    Compensated {
        compensation_result: String,
    },
    Pending,
}

/// Resume recommendation after transaction interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxResumeRecommendation {
    ContinueFromCheckpoint,
    RestartFresh,
    CompensateAndAbort,
    AlreadyComplete,
}

/// Per-step execution record in a transaction ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxStepRecordData {
    pub ordinal: u64,
    pub step_id: String,
    pub idem_key: String,
    pub execution_id: String,
    pub timestamp_ms: u64,
    pub outcome: TxStepOutcome,
    pub risk: TxStepRisk,
    pub prev_hash: String,
    pub agent_id: String,
}

/// Chain verification result for idempotency integrity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxChainVerificationData {
    pub chain_intact: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_break_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_ordinals: Vec<u64>,
    pub total_records: usize,
}

/// Response data for `ft robot tx run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRunData {
    pub execution_id: String,
    pub plan_id: String,
    pub plan_hash: u64,
    pub phase: TxPhaseState,
    pub step_count: usize,
    pub completed_count: usize,
    pub failed_count: usize,
    pub skipped_count: usize,
    pub records: Vec<TxStepRecordData>,
    pub chain_verification: TxChainVerificationData,
}

/// Resume context for interrupted transactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxResumeData {
    pub execution_id: String,
    pub plan_id: String,
    pub interrupted_phase: TxPhaseState,
    pub completed_steps: Vec<String>,
    pub failed_steps: Vec<String>,
    pub remaining_steps: Vec<String>,
    pub compensated_steps: Vec<String>,
    pub chain_intact: bool,
    pub last_hash: String,
    pub recommendation: TxResumeRecommendation,
}

/// Response data for `ft robot tx rollback`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRollbackData {
    pub execution_id: String,
    pub plan_id: String,
    pub phase: TxPhaseState,
    pub compensated_steps: Vec<String>,
    pub failed_compensations: Vec<String>,
    pub total_compensated: usize,
    pub total_failed: usize,
    pub chain_verification: TxChainVerificationData,
}

/// Forensic bundle classification level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxBundleClassification {
    Internal,
    TeamReview,
    ExternalAudit,
}

/// Timeline entry in a forensic bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxTimelineEntryData {
    pub timestamp_ms: u64,
    pub phase: String,
    pub step_id: String,
    pub kind: String,
    pub reason_code: String,
    pub summary: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    pub record_hash: String,
}

/// Response data for `ft robot tx show`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxShowData {
    pub execution_id: String,
    pub plan_id: String,
    pub plan_hash: u64,
    pub phase: TxPhaseState,
    pub classification: TxBundleClassification,
    pub step_count: usize,
    pub record_count: usize,
    pub high_risk_count: usize,
    pub critical_risk_count: usize,
    pub overall_risk: TxStepRisk,
    pub chain_intact: bool,
    pub timeline: Vec<TxTimelineEntryData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<TxResumeData>,
    pub records: Vec<TxStepRecordData>,
    pub redacted_field_count: usize,
}

// ============================================================================
// Workflows
// ============================================================================

/// Response data for `ft robot workflow run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRunData {
    pub workflow_name: String,
    pub pane_id: u64,
    #[serde(default)]
    pub execution_id: Option<String>,
    pub status: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub step_index: Option<usize>,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
}

/// Response data for `ft robot workflow list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowListData {
    pub workflows: Vec<WorkflowInfo>,
    pub total: usize,
    #[serde(default)]
    pub enabled_count: Option<usize>,
}

/// Individual workflow info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInfo {
    pub name: String,
    pub enabled: bool,
    #[serde(default)]
    pub trigger_event_types: Option<Vec<String>>,
    #[serde(default)]
    pub requires_pane: Option<bool>,
}

/// Response data for `ft robot workflow status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStatusData {
    pub execution_id: String,
    pub workflow_name: String,
    #[serde(default)]
    pub pane_id: Option<u64>,
    #[serde(default)]
    pub trigger_event_id: Option<i64>,
    pub status: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub current_step: Option<usize>,
    #[serde(default)]
    pub total_steps: Option<usize>,
    #[serde(default)]
    pub plan: Option<serde_json::Value>,
    #[serde(default)]
    pub created_at: Option<i64>,
}

/// Response data for workflow status list (--pane or --active).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStatusListData {
    pub executions: Vec<WorkflowStatusData>,
    #[serde(default)]
    pub pane_filter: Option<u64>,
    #[serde(default)]
    pub active_only: Option<bool>,
    pub count: usize,
}

/// Response data for `ft robot workflow abort`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowAbortData {
    pub execution_id: String,
    pub aborted: bool,
    pub forced: bool,
    #[serde(default)]
    pub workflow_name: Option<String>,
    #[serde(default)]
    pub previous_status: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

// ============================================================================
// Rules
// ============================================================================

/// Response data for `ft robot rules list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesListData {
    pub rules: Vec<RuleItem>,
    #[serde(default)]
    pub pack_filter: Option<String>,
    #[serde(default)]
    pub agent_type_filter: Option<String>,
}

/// Individual rule item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleItem {
    pub id: String,
    pub agent_type: String,
    pub event_type: String,
    pub severity: String,
    pub description: String,
    #[serde(default)]
    pub workflow: Option<String>,
    pub anchor_count: usize,
    pub has_regex: bool,
}

/// Response data for `ft robot rules test`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesTestData {
    pub text_length: usize,
    pub match_count: usize,
    pub matches: Vec<RuleMatchItem>,
}

/// Individual rule match item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleMatchItem {
    pub rule_id: String,
    pub start: usize,
    pub end: usize,
    pub matched_text: String,
    #[serde(default)]
    pub trace: Option<RuleTraceInfo>,
}

/// Trace info for rule match debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleTraceInfo {
    pub anchors_checked: bool,
    pub regex_matched: bool,
}

/// Response data for `ft robot rules show`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDetailData {
    pub id: String,
    pub agent_type: String,
    pub event_type: String,
    pub severity: String,
    pub description: String,
    pub anchors: Vec<String>,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    #[serde(default)]
    pub manual_fix: Option<String>,
    #[serde(default)]
    pub learn_more_url: Option<String>,
}

/// Response data for `ft robot rules lint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesLintData {
    pub total_rules: usize,
    pub rules_checked: usize,
    pub errors: Vec<LintIssue>,
    pub warnings: Vec<LintIssue>,
    #[serde(default)]
    pub fixture_coverage: Option<FixtureCoverage>,
    pub passed: bool,
}

/// Individual lint issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintIssue {
    pub rule_id: String,
    pub category: String,
    pub message: String,
    #[serde(default)]
    pub suggestion: Option<String>,
}

/// Fixture coverage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureCoverage {
    pub rules_with_fixtures: usize,
    pub rules_without_fixtures: Vec<String>,
    pub total_fixtures: usize,
}

// ============================================================================
// Accounts
// ============================================================================

/// Response data for `ft robot accounts list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsListData {
    pub accounts: Vec<AccountInfo>,
    pub total: usize,
    pub service: String,
    #[serde(default)]
    pub pick_preview: Option<AccountPickPreview>,
}

/// Individual account info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountInfo {
    pub account_id: String,
    pub service: String,
    #[serde(default)]
    pub name: Option<String>,
    pub percent_remaining: f64,
    #[serde(default)]
    pub reset_at: Option<String>,
    #[serde(default)]
    pub tokens_used: Option<i64>,
    #[serde(default)]
    pub tokens_remaining: Option<i64>,
    #[serde(default)]
    pub tokens_limit: Option<i64>,
    pub last_refreshed_at: i64,
    #[serde(default)]
    pub last_used_at: Option<i64>,
}

/// Pick preview showing which account would be selected next.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountPickPreview {
    #[serde(default)]
    pub selected_account_id: Option<String>,
    #[serde(default)]
    pub selected_name: Option<String>,
    pub selection_reason: String,
    pub threshold_percent: f64,
    pub candidates_count: usize,
    pub filtered_count: usize,
    #[serde(default)]
    pub quota_advisory: Option<AccountQuotaAdvisoryInfo>,
}

/// Quota advisory details returned with account pick previews.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountQuotaAdvisoryInfo {
    pub availability: String,
    pub low_quota_threshold_percent: f64,
    #[serde(default)]
    pub selected_percent_remaining: Option<f64>,
    #[serde(default)]
    pub warning: Option<String>,
    pub blocking: bool,
}

/// Response data for `ft robot accounts refresh`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsRefreshData {
    pub service: String,
    pub refreshed_count: usize,
    #[serde(default)]
    pub refreshed_at: Option<String>,
    pub accounts: Vec<AccountInfo>,
}

// ============================================================================
// Reservations
// ============================================================================

/// Response data for `ft robot reserve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReserveData {
    pub reservation: ReservationInfo,
}

/// Response data for `ft robot release`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseData {
    pub reservation_id: i64,
    pub released: bool,
}

/// Response data for `ft robot reservations list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationsListData {
    pub reservations: Vec<ReservationInfo>,
    pub total: usize,
}

/// Individual reservation info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationInfo {
    pub id: i64,
    pub pane_id: u64,
    pub owner_kind: String,
    pub owner_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(default)]
    pub released_at: Option<i64>,
    pub status: String,
}

// ============================================================================
// Meta / Diagnostics
// ============================================================================

/// Response data for `ft robot why`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhyData {
    pub code: String,
    pub category: String,
    pub title: String,
    pub explanation: String,
    #[serde(default)]
    pub suggestions: Option<Vec<String>>,
    #[serde(default)]
    pub see_also: Option<Vec<String>>,
}

/// Response data for `ft robot approve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproveData {
    pub code: String,
    pub valid: bool,
    #[serde(default)]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub action_kind: Option<String>,
    #[serde(default)]
    pub pane_id: Option<u64>,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub action_fingerprint: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// Response data for `ft robot quick-start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartData {
    pub description: String,
    pub global_flags: Vec<QuickStartGlobalFlag>,
    pub core_loop: Vec<QuickStartStep>,
    pub commands: Vec<QuickStartCommand>,
    pub tips: Vec<String>,
    pub error_handling: QuickStartErrorHandling,
}

/// Global flag for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartGlobalFlag {
    pub flag: String,
    #[serde(default)]
    pub env_var: Option<String>,
    pub description: String,
}

/// Step in the core loop for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartStep {
    pub step: u8,
    pub action: String,
    pub command: String,
}

/// Command entry for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartCommand {
    pub name: String,
    pub args: String,
    pub summary: String,
    pub examples: Vec<String>,
}

/// Error handling section for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartErrorHandling {
    pub common_codes: Vec<QuickStartErrorCode>,
    pub safety_notes: Vec<String>,
}

/// Common error code entry for quick-start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickStartErrorCode {
    pub code: String,
    pub meaning: String,
    pub recovery: String,
}

// ============================================================================
// Convenience parsing
// ============================================================================

/// Parse a raw JSON string into a typed `RobotResponse<T>`.
///
/// This is a convenience wrapper around `serde_json::from_str`.
pub fn parse_response<T: serde::de::DeserializeOwned>(
    json: &str,
) -> Result<RobotResponse<T>, serde_json::Error> {
    serde_json::from_str(json)
}

/// Parse a raw JSON string into a `RobotResponse<serde_json::Value>` for
/// untyped access when the data type is not known at compile time.
pub fn parse_response_untyped(
    json: &str,
) -> Result<RobotResponse<serde_json::Value>, serde_json::Error> {
    serde_json::from_str(json)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Envelope parsing ---------------------------------------------------

    #[test]
    fn parse_success_envelope() {
        let json = json!({
            "ok": true,
            "data": {"pane_id": 1, "text": "hello", "tail_lines": 100, "escapes_included": false},
            "elapsed_ms": 5,
            "version": "0.1.0",
            "now": 1700000000000u64
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data.pane_id, 1);
        assert_eq!(data.text, "hello");
        assert_eq!(data.tail_lines, 100);
        assert!(!data.escapes_included);
        assert!(!data.truncated);
        assert!(data.truncation_info.is_none());
    }

    #[test]
    fn parse_error_envelope() {
        let json = json!({
            "ok": false,
            "error": "pane 42 not found",
            "error_code": "FT-1003",
            "hint": "check ft list-panes",
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 1700000000000u64
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        assert!(!resp.ok);
        assert!(resp.data.is_none());
        assert_eq!(resp.error.as_deref(), Some("pane 42 not found"));
        assert_eq!(resp.error_code.as_deref(), Some("FT-1003"));
        assert_eq!(resp.hint.as_deref(), Some("check ft list-panes"));
    }

    #[test]
    fn into_result_ok() {
        let json = json!({
            "ok": true,
            "data": {"pane_id": 1, "text": "x", "tail_lines": 1, "escapes_included": false},
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.text, "x");
    }

    #[test]
    fn into_result_err() {
        let json = json!({
            "ok": false,
            "error": "denied",
            "error_code": "FT-4001",
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.message, "denied");
        assert_eq!(err.code.as_deref(), Some("FT-4001"));
        assert!(err.to_string().contains("FT-4001"));
    }

    #[test]
    fn into_result_ok_null_data() {
        let json = json!({
            "ok": true,
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let err = resp.into_result().unwrap_err();
        assert!(err.message.contains("null"));
    }

    // -- Error code parsing -------------------------------------------------

    #[test]
    fn error_code_roundtrip() {
        for code in ["FT-1001", "FT-2003", "FT-4001", "FT-5004", "FT-9003"] {
            let parsed = ErrorCode::parse(code).unwrap();
            assert_eq!(parsed.as_str(), code);
        }
    }

    #[test]
    fn error_code_unknown() {
        let parsed = ErrorCode::parse("FT-8888").unwrap();
        assert_eq!(parsed, ErrorCode::Unknown(8888));
        assert_eq!(parsed.number(), 8888);
    }

    #[test]
    fn error_code_invalid_prefix() {
        assert!(ErrorCode::parse("XX-1001").is_none());
        assert!(ErrorCode::parse("garbage").is_none());
    }

    #[test]
    fn error_code_categories() {
        assert_eq!(
            ErrorCode::WeztermNotFound.category(),
            ErrorCategory::Wezterm
        );
        assert_eq!(ErrorCode::DatabaseLocked.category(), ErrorCategory::Storage);
        assert_eq!(ErrorCode::InvalidRegex.category(), ErrorCategory::Pattern);
        assert_eq!(ErrorCode::ActionDenied.category(), ErrorCategory::Policy);
        assert_eq!(
            ErrorCode::WorkflowNotFound.category(),
            ErrorCategory::Workflow
        );
        assert_eq!(ErrorCode::NetworkTimeout.category(), ErrorCategory::Network);
        assert_eq!(ErrorCode::ConfigInvalid.category(), ErrorCategory::Config);
        assert_eq!(ErrorCode::InternalError.category(), ErrorCategory::Internal);
    }

    #[test]
    fn error_code_retryable() {
        assert!(ErrorCode::DatabaseLocked.is_retryable());
        assert!(ErrorCode::RateLimitExceeded.is_retryable());
        assert!(ErrorCode::NetworkTimeout.is_retryable());
        assert!(!ErrorCode::ActionDenied.is_retryable());
        assert!(!ErrorCode::ConfigInvalid.is_retryable());
        assert!(!ErrorCode::InternalError.is_retryable());
    }

    #[test]
    fn parsed_error_code_from_response() {
        let json = json!({
            "ok": false,
            "error": "locked",
            "error_code": "FT-2001",
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        assert_eq!(resp.parsed_error_code(), Some(ErrorCode::DatabaseLocked));
    }

    // -- Data type parsing --------------------------------------------------

    #[test]
    fn parse_get_text_with_truncation() {
        let json = json!({
            "ok": true,
            "data": {
                "pane_id": 3,
                "text": "output...",
                "tail_lines": 50,
                "escapes_included": true,
                "truncated": true,
                "truncation_info": {
                    "original_bytes": 10000,
                    "returned_bytes": 5000,
                    "original_lines": 200,
                    "returned_lines": 50
                }
            },
            "elapsed_ms": 12,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.truncated);
        let info = data.truncation_info.unwrap();
        assert_eq!(info.original_bytes, 10000);
        assert_eq!(info.returned_lines, 50);
    }

    #[test]
    fn parse_batch_get_text_data() {
        let json = json!({
            "ok": true,
            "data": {
                "pane_ids": [0, 1],
                "tail_lines": 20,
                "escapes_included": false,
                "results": {
                    "0": {
                        "status": "ok",
                        "text": "ready",
                        "truncated": false
                    },
                    "1": {
                        "status": "error",
                        "code": "robot.pane_not_found",
                        "message": "Pane not found: 1",
                        "hint": "Use 'ft robot state' to list panes."
                    }
                }
            },
            "elapsed_ms": 4,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<BatchGetTextData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.pane_ids, vec![0, 1]);
        match data.results.get(&0).unwrap() {
            PaneTextResult::Ok { text, .. } => assert_eq!(text, "ready"),
            other @ PaneTextResult::Error { .. } => panic!("expected ok variant, got {other:?}"),
        }
        match data.results.get(&1).unwrap() {
            PaneTextResult::Error { code, .. } => assert_eq!(code, "robot.pane_not_found"),
            other @ PaneTextResult::Ok { .. } => panic!("expected error variant, got {other:?}"),
        }
    }

    #[test]
    fn parse_state_with_text_data() {
        let json = json!({
            "ok": true,
            "data": {
                "panes": [{
                    "pane_id": 2,
                    "pane_uuid": "abc-123",
                    "tab_id": 10,
                    "window_id": 11,
                    "domain": "local",
                    "title": "shell",
                    "cwd": "/tmp",
                    "observed": true
                }],
                "tail_lines": 10,
                "escapes_included": true,
                "pane_text": {
                    "2": {
                        "status": "ok",
                        "text": "line1\\nline2",
                        "truncated": true,
                        "truncation_info": {
                            "original_bytes": 1000,
                            "returned_bytes": 200,
                            "original_lines": 100,
                            "returned_lines": 10
                        }
                    }
                }
            },
            "elapsed_ms": 9,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<StateWithTextData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.panes.len(), 1);
        assert_eq!(data.panes[0].pane_id, 2);
        assert_eq!(data.tail_lines, 10);
        match data.pane_text.get(&2).unwrap() {
            PaneTextResult::Ok { truncated, .. } => assert!(*truncated),
            other @ PaneTextResult::Error { .. } => panic!("expected ok variant, got {other:?}"),
        }
    }

    #[test]
    fn parse_wait_for_data() {
        let json = json!({
            "ok": true,
            "data": {
                "pane_id": 1,
                "pattern": "\\$",
                "matched": true,
                "elapsed_ms": 500,
                "polls": 10,
                "is_regex": true
            },
            "elapsed_ms": 510,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WaitForData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.matched);
        assert!(data.is_regex);
        assert_eq!(data.polls, 10);
    }

    #[test]
    fn parse_search_data() {
        let json = json!({
            "ok": true,
            "data": {
                "query": "error",
                "results": [{
                    "segment_id": 1,
                    "pane_id": 2,
                    "seq": 5,
                    "captured_at": 1700000000000i64,
                    "score": 1.5,
                    "snippet": "...error occurred..."
                }],
                "total_hits": 1,
                "limit": 20
            },
            "elapsed_ms": 3,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<SearchData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.total_hits, 1);
        assert!((data.results[0].score - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_events_data() {
        let json = json!({
            "ok": true,
            "data": {
                "events": [{
                    "id": 42,
                    "pane_id": 1,
                    "rule_id": "codex.build_error",
                    "pack_id": "codex",
                    "event_type": "error",
                    "severity": "high",
                    "confidence": 0.95,
                    "captured_at": 1700000000000i64
                }],
                "total_count": 1,
                "limit": 50,
                "unhandled_only": false
            },
            "elapsed_ms": 8,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<EventsData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.events.len(), 1);
        assert_eq!(data.events[0].rule_id, "codex.build_error");
        assert!((data.events[0].confidence - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_workflow_run_data() {
        let json = json!({
            "ok": true,
            "data": {
                "workflow_name": "fix_build",
                "pane_id": 1,
                "execution_id": "exec-abc",
                "status": "running",
                "started_at": 1700000000000i64
            },
            "elapsed_ms": 15,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WorkflowRunData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.workflow_name, "fix_build");
        assert_eq!(data.status, "running");
    }

    #[test]
    fn parse_workflow_list_data() {
        let json = json!({
            "ok": true,
            "data": {
                "workflows": [
                    {"name": "fix_build", "enabled": true},
                    {"name": "notify", "enabled": false}
                ],
                "total": 2,
                "enabled_count": 1
            },
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WorkflowListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.total, 2);
        assert!(data.workflows[0].enabled);
        assert!(!data.workflows[1].enabled);
    }

    #[test]
    fn parse_rules_list_data() {
        let json = json!({
            "ok": true,
            "data": {
                "rules": [{
                    "id": "codex.build_error",
                    "agent_type": "codex",
                    "event_type": "error",
                    "severity": "high",
                    "description": "Build error detected",
                    "anchor_count": 3,
                    "has_regex": true
                }],
                "pack_filter": "codex"
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<RulesListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.rules[0].id, "codex.build_error");
        assert!(data.rules[0].has_regex);
    }

    #[test]
    fn parse_rules_test_data() {
        let json = json!({
            "ok": true,
            "data": {
                "text_length": 500,
                "match_count": 2,
                "matches": [
                    {
                        "rule_id": "codex.build_error",
                        "start": 10,
                        "end": 30,
                        "matched_text": "error: cannot find",
                        "trace": {
                            "anchors_checked": true,
                            "regex_matched": true
                        }
                    }
                ]
            },
            "elapsed_ms": 5,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<RulesTestData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.match_count, 2);
        assert!(data.matches[0].trace.as_ref().unwrap().regex_matched);
    }

    #[test]
    fn parse_why_data() {
        let json = json!({
            "ok": true,
            "data": {
                "code": "FT-2001",
                "category": "storage",
                "title": "Database locked",
                "explanation": "SQLite database is locked by another process.",
                "suggestions": ["retry after 1s", "check for hung wa processes"]
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WhyData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.code, "FT-2001");
        assert_eq!(data.suggestions.unwrap().len(), 2);
    }

    #[test]
    fn parse_approve_data() {
        let json = json!({
            "ok": true,
            "data": {
                "code": "AP-abc123",
                "valid": true,
                "created_at": 1700000000000u64,
                "action_kind": "send_text",
                "pane_id": 1,
                "expires_at": 1700000060000u64
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<ApproveData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.valid);
        assert_eq!(data.action_kind.as_deref(), Some("send_text"));
    }

    #[test]
    fn parse_accounts_list_data() {
        let json = json!({
            "ok": true,
            "data": {
                "accounts": [{
                    "account_id": "acc-1",
                    "service": "anthropic",
                    "percent_remaining": 85.5,
                    "last_refreshed_at": 1700000000000i64
                }],
                "total": 1,
                "service": "anthropic",
                "pick_preview": {
                    "selected_account_id": "acc-1",
                    "selected_name": "Primary",
                    "selection_reason": "Highest percent_remaining (85.5%)",
                    "threshold_percent": 5.0,
                    "candidates_count": 1,
                    "filtered_count": 0,
                    "quota_advisory": {
                        "availability": "available",
                        "low_quota_threshold_percent": 10.0,
                        "selected_percent_remaining": 85.5,
                        "blocking": false
                    }
                }
            },
            "elapsed_ms": 3,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<AccountsListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!((data.accounts[0].percent_remaining - 85.5).abs() < f64::EPSILON);
        let pick = data.pick_preview.expect("pick preview should parse");
        let advisory = pick.quota_advisory.expect("quota advisory should parse");
        assert_eq!(advisory.availability, "available");
        assert!(!advisory.blocking);
    }

    #[test]
    fn parse_reservations_list_data() {
        let json = json!({
            "ok": true,
            "data": {
                "reservations": [{
                    "id": 1,
                    "pane_id": 5,
                    "owner_kind": "agent",
                    "owner_id": "codex-1",
                    "reason": "build monitoring",
                    "created_at": 1700000000000i64,
                    "expires_at": 1700000060000i64,
                    "status": "active"
                }],
                "total": 1
            },
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<ReservationsListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.reservations[0].status, "active");
        assert_eq!(
            data.reservations[0].reason.as_deref(),
            Some("build monitoring")
        );
    }

    #[test]
    fn parse_untyped_response() {
        let raw = r#"{"ok":true,"data":{"foo":"bar"},"elapsed_ms":1,"version":"0.1.0","now":0}"#;
        let resp = parse_response_untyped(raw).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.data.unwrap()["foo"], "bar");
    }

    #[test]
    fn from_json_convenience() {
        let raw = r#"{"ok":true,"data":{"pane_id":1,"text":"hi","tail_lines":10,"escapes_included":false},"elapsed_ms":1,"version":"0.1.0","now":0}"#;
        let resp = RobotResponse::<GetTextData>::from_json(raw).unwrap();
        assert_eq!(resp.data.unwrap().text, "hi");
    }

    #[test]
    fn tolerant_of_missing_optional_fields() {
        // Minimal envelope with only required fields in data
        let json = json!({
            "ok": true,
            "data": {
                "events": [],
                "total_count": 0,
                "limit": 20
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<EventsData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.pane_filter.is_none());
        assert!(!data.unhandled_only);
        assert!(!data.would_handle);
    }

    #[test]
    fn robot_error_display() {
        let err = RobotError {
            code: Some("FT-1003".to_string()),
            message: "pane not found".to_string(),
            hint: Some("check pane id".to_string()),
        };
        assert_eq!(err.to_string(), "[FT-1003] pane not found");

        let err_no_code = RobotError {
            code: None,
            message: "something failed".to_string(),
            hint: None,
        };
        assert_eq!(err_no_code.to_string(), "something failed");
    }

    #[test]
    fn quick_start_data_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "description": "Quick start guide",
                "global_flags": [{"flag": "--pane", "env_var": "FT_PANE", "description": "target pane"}],
                "core_loop": [{"step": 1, "action": "get text", "command": "ft robot get-text"}],
                "commands": [{
                    "name": "get-text",
                    "args": "--pane <ID>",
                    "summary": "Get pane text",
                    "examples": ["ft robot get-text --pane 1"]
                }],
                "tips": ["use --format json"],
                "error_handling": {
                    "common_codes": [{"code": "FT-1003", "meaning": "pane not found", "recovery": "check id"}],
                    "safety_notes": ["always check ok field"]
                }
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<QuickStartData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.global_flags.len(), 1);
        assert_eq!(data.core_loop[0].step, 1);
        assert_eq!(data.commands[0].name, "get-text");
        assert_eq!(data.error_handling.common_codes[0].code, "FT-1003");
    }

    #[test]
    fn workflow_abort_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "execution_id": "exec-xyz",
                "aborted": true,
                "forced": false,
                "workflow_name": "fix_build",
                "previous_status": "running"
            },
            "elapsed_ms": 5,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WorkflowAbortData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.aborted);
        assert!(!data.forced);
    }

    #[test]
    fn rules_lint_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "total_rules": 50,
                "rules_checked": 48,
                "errors": [],
                "warnings": [{"rule_id": "x.y", "category": "style", "message": "no desc"}],
                "passed": true
            },
            "elapsed_ms": 30,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<RulesLintData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.passed);
        assert_eq!(data.warnings.len(), 1);
    }

    #[test]
    fn event_mutation_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "event_id": 99,
                "changed": true,
                "annotations": {"triage_state": "resolved"}
            },
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<EventMutationData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.event_id, 99);
        assert_eq!(data.changed, Some(true));
    }

    #[test]
    fn rule_detail_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "id": "codex.build_error",
                "agent_type": "codex",
                "event_type": "error",
                "severity": "high",
                "description": "Build error detected",
                "anchors": ["error:", "failed"],
                "regex": "error\\[E\\d+\\]",
                "workflow": "fix_build"
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<RuleDetailData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.anchors.len(), 2);
        assert!(data.regex.is_some());
    }

    #[test]
    fn workflow_status_list_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "executions": [{
                    "execution_id": "exec-1",
                    "workflow_name": "fix_build",
                    "status": "completed"
                }],
                "count": 1,
                "active_only": true
            },
            "elapsed_ms": 3,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WorkflowStatusListData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.count, 1);
        assert_eq!(data.executions[0].status, "completed");
    }

    #[test]
    fn send_data_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "pane_id": 1,
                "injection": {"status": "allowed", "summary": "echo hello", "pane_id": 1, "action": "send_text", "decision": {"decision": "allow"}}
            },
            "elapsed_ms": 50,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<SendData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.pane_id, 1);
        assert!(data.injection.is_object());
    }

    #[test]
    fn reserve_data_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "reservation": {
                    "id": 7,
                    "pane_id": 3,
                    "owner_kind": "agent",
                    "owner_id": "codex-1",
                    "created_at": 1700000000000i64,
                    "expires_at": 1700000060000i64,
                    "status": "active"
                }
            },
            "elapsed_ms": 4,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<ReserveData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.reservation.id, 7);
        assert_eq!(data.reservation.status, "active");
    }

    #[test]
    fn release_data_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "reservation_id": 7,
                "released": true
            },
            "elapsed_ms": 2,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<ReleaseData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.released);
    }

    #[test]
    fn accounts_refresh_parses() {
        let json = json!({
            "ok": true,
            "data": {
                "service": "anthropic",
                "refreshed_count": 2,
                "refreshed_at": "2025-01-01T00:00:00Z",
                "accounts": [
                    {"account_id": "a1", "service": "anthropic", "percent_remaining": 90.0, "last_refreshed_at": 0},
                    {"account_id": "a2", "service": "anthropic", "percent_remaining": 50.0, "last_refreshed_at": 0}
                ]
            },
            "elapsed_ms": 100,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<AccountsRefreshData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.refreshed_count, 2);
        assert_eq!(data.accounts.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Batch — RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    #[test]
    fn from_json_bytes_convenience() {
        let raw = br#"{"ok":true,"data":{"pane_id":5,"text":"bytes","tail_lines":1,"escapes_included":true},"elapsed_ms":2,"version":"0.2.0","now":99}"#;
        let resp = RobotResponse::<GetTextData>::from_json_bytes(raw).unwrap();
        let data = resp.data.unwrap();
        assert_eq!(data.pane_id, 5);
        assert_eq!(data.text, "bytes");
        assert!(data.escapes_included);
    }

    #[test]
    fn into_result_err_no_message_uses_fallback() {
        // Error response with no `error` field should produce "unknown error".
        let json = json!({
            "ok": false,
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.message, "unknown error");
        assert!(err.code.is_none());
        assert!(err.hint.is_none());
    }

    #[test]
    fn into_result_err_preserves_hint() {
        let json = json!({
            "ok": false,
            "error": "rate limited",
            "error_code": "FT-4002",
            "hint": "wait 60 seconds",
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.hint.as_deref(), Some("wait 60 seconds"));
        assert_eq!(err.message, "rate limited");
    }

    #[test]
    fn parsed_error_code_returns_none_when_absent() {
        let json = json!({
            "ok": true,
            "data": {"pane_id": 1, "text": "ok", "tail_lines": 1, "escapes_included": false},
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        assert!(resp.parsed_error_code().is_none());
    }

    #[test]
    fn parsed_error_code_returns_none_for_invalid_prefix() {
        let json = json!({
            "ok": false,
            "error": "bad",
            "error_code": "XX-9999",
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<GetTextData> = serde_json::from_value(json).unwrap();
        assert!(resp.parsed_error_code().is_none());
    }

    #[test]
    fn error_code_parse_non_numeric_suffix() {
        assert!(ErrorCode::parse("FT-abc").is_none());
        assert!(ErrorCode::parse("FT-").is_none());
        assert!(ErrorCode::parse("FT-12.5").is_none());
    }

    #[test]
    fn error_code_display_trait() {
        assert_eq!(format!("{}", ErrorCode::PaneNotFound), "FT-1003");
        assert_eq!(format!("{}", ErrorCode::DiskFull), "FT-2005");
        assert_eq!(format!("{}", ErrorCode::Unknown(7777)), "FT-7777");
    }

    #[test]
    fn error_code_unknown_category_is_internal() {
        // Unknown codes with number >= 9000 or in unmapped ranges
        // fall into the Internal category via the catch-all.
        let code = ErrorCode::Unknown(0);
        assert_eq!(code.category(), ErrorCategory::Internal);

        let code2 = ErrorCode::Unknown(8500);
        assert_eq!(code2.category(), ErrorCategory::Internal);
    }

    #[test]
    fn error_code_retryable_exhaustive() {
        // All 6 retryable codes
        assert!(ErrorCode::DatabaseLocked.is_retryable());
        assert!(ErrorCode::RateLimitExceeded.is_retryable());
        assert!(ErrorCode::NetworkTimeout.is_retryable());
        assert!(ErrorCode::ConnectionRefused.is_retryable());
        assert!(ErrorCode::PatternTimeout.is_retryable());
        assert!(ErrorCode::WeztermConnectionRefused.is_retryable());

        // Spot-check a selection of non-retryable codes
        let non_retryable = [
            ErrorCode::WeztermNotFound,
            ErrorCode::WeztermExecFailed,
            ErrorCode::PaneNotFound,
            ErrorCode::WeztermParseFailed,
            ErrorCode::StorageCorruption,
            ErrorCode::FtsIndexError,
            ErrorCode::MigrationFailed,
            ErrorCode::DiskFull,
            ErrorCode::InvalidRegex,
            ErrorCode::RulePackNotFound,
            ErrorCode::ActionDenied,
            ErrorCode::ApprovalRequired,
            ErrorCode::ApprovalExpired,
            ErrorCode::WorkflowNotFound,
            ErrorCode::WorkflowStepFailed,
            ErrorCode::WorkflowTimeout,
            ErrorCode::WorkflowAlreadyRunning,
            ErrorCode::ConfigInvalid,
            ErrorCode::ConfigNotFound,
            ErrorCode::InternalError,
            ErrorCode::FeatureNotAvailable,
            ErrorCode::VersionMismatch,
            ErrorCode::Unknown(9999),
        ];
        for code in non_retryable {
            assert!(!code.is_retryable(), "expected non-retryable: {}", code);
        }
    }

    #[test]
    fn error_code_clone_copy_eq_hash() {
        let a = ErrorCode::DatabaseLocked;
        let b = a; // Copy
        let c = a; // Clone (Copy type)
        assert_eq!(a, b);
        assert_eq!(b, c);

        // Verify Hash works by inserting into a HashSet
        let mut set = std::collections::HashSet::new();
        set.insert(a);
        set.insert(b);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn robot_error_implements_std_error() {
        let err = RobotError {
            code: Some("FT-9001".to_string()),
            message: "internal".to_string(),
            hint: None,
        };
        // Verify the Error trait is implemented (source returns None by default)
        let as_error: &dyn std::error::Error = &err;
        assert!(as_error.source().is_none());
    }

    #[test]
    fn envelope_serialize_deserialize_roundtrip() {
        let original = RobotResponse {
            ok: true,
            data: Some(GetTextData {
                pane_id: 42,
                text: "roundtrip".to_string(),
                tail_lines: 200,
                escapes_included: true,
                truncated: false,
                truncation_info: None,
            }),
            error: None,
            error_code: None,
            hint: None,
            elapsed_ms: 7,
            version: "1.0.0".to_string(),
            now: 1700000000000,
        };
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: RobotResponse<GetTextData> = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.ok, original.ok);
        assert_eq!(deserialized.elapsed_ms, 7);
        assert_eq!(deserialized.version, "1.0.0");
        let data = deserialized.data.unwrap();
        assert_eq!(data.pane_id, 42);
        assert_eq!(data.text, "roundtrip");
    }

    #[test]
    fn pane_state_data_minimal_optional_fields() {
        let json = json!({
            "pane_id": 10,
            "tab_id": 20,
            "window_id": 30,
            "domain": "ssh"
        });
        let psd: PaneStateData = serde_json::from_value(json).unwrap();
        assert_eq!(psd.pane_id, 10);
        assert_eq!(psd.domain, "ssh");
        assert!(psd.pane_uuid.is_none());
        assert!(psd.title.is_none());
        assert!(psd.cwd.is_none());
        assert!(!psd.observed);
        assert!(psd.ignore_reason.is_none());
    }

    #[test]
    fn search_hit_with_semantic_and_fusion_fields() {
        let json = json!({
            "ok": true,
            "data": {
                "query": "deploy",
                "results": [{
                    "segment_id": 10,
                    "pane_id": 3,
                    "seq": 100,
                    "captured_at": 1700000000000i64,
                    "score": 2.5,
                    "snippet": "deploying...",
                    "content": "full content here",
                    "semantic_score": 0.87,
                    "fusion_rank": 1
                }],
                "total_hits": 1,
                "limit": 10,
                "mode": "hybrid",
                "pane_filter": 3,
                "since_filter": 1699999000000i64,
                "until_filter": 1700001000000i64
            },
            "elapsed_ms": 15,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<SearchData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.mode.as_deref(), Some("hybrid"));
        assert_eq!(data.pane_filter, Some(3));
        assert_eq!(data.since_filter, Some(1699999000000));
        assert_eq!(data.until_filter, Some(1700001000000));
        let hit = &data.results[0];
        assert!((hit.semantic_score.unwrap() - 0.87).abs() < f64::EPSILON);
        assert_eq!(hit.fusion_rank, Some(1));
        assert_eq!(hit.content.as_deref(), Some("full content here"));
    }

    #[test]
    fn event_item_with_would_handle_with() {
        let json = json!({
            "ok": true,
            "data": {
                "events": [{
                    "id": 100,
                    "pane_id": 5,
                    "rule_id": "test.timeout",
                    "pack_id": "test",
                    "event_type": "warning",
                    "severity": "medium",
                    "confidence": 0.80,
                    "captured_at": 1700000000000i64,
                    "handled_at": 1700000010000i64,
                    "workflow_id": "wf-99",
                    "extracted": {"key": "val"},
                    "annotations": {"label": "timeout"},
                    "would_handle_with": {
                        "workflow": "restart_service",
                        "preview_command": "systemctl restart app",
                        "first_step": "check_health",
                        "estimated_duration_ms": 5000,
                        "would_run": true,
                        "reason": "auto policy"
                    }
                }],
                "total_count": 1,
                "limit": 10,
                "would_handle": true,
                "dry_run": true
            },
            "elapsed_ms": 20,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<EventsData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(data.would_handle);
        assert!(data.dry_run);
        let ev = &data.events[0];
        assert_eq!(ev.handled_at, Some(1700000010000));
        assert_eq!(ev.workflow_id.as_deref(), Some("wf-99"));
        let wh = ev.would_handle_with.as_ref().unwrap();
        assert_eq!(wh.workflow, "restart_service");
        assert_eq!(wh.first_step.as_deref(), Some("check_health"));
        assert_eq!(wh.estimated_duration_ms, Some(5000));
        assert_eq!(wh.would_run, Some(true));
    }

    #[test]
    fn workflow_info_with_trigger_event_types() {
        let json = json!({
            "name": "auto_fix",
            "enabled": true,
            "trigger_event_types": ["error", "warning"],
            "requires_pane": true
        });
        let info: WorkflowInfo = serde_json::from_value(json).unwrap();
        assert!(info.enabled);
        assert_eq!(
            info.trigger_event_types.as_ref().unwrap(),
            &["error", "warning"]
        );
        assert_eq!(info.requires_pane, Some(true));
    }

    #[test]
    fn workflow_status_data_full_parse() {
        let json = json!({
            "ok": true,
            "data": {
                "execution_id": "exec-full",
                "workflow_name": "deploy",
                "pane_id": 8,
                "trigger_event_id": 55,
                "status": "running",
                "message": "step 2 of 4",
                "started_at": 1700000000000i64,
                "completed_at": null,
                "current_step": 2,
                "total_steps": 4,
                "plan": {"steps": ["a", "b", "c", "d"]},
                "created_at": 1699999990000i64
            },
            "elapsed_ms": 5,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WorkflowStatusData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.execution_id, "exec-full");
        assert_eq!(data.pane_id, Some(8));
        assert_eq!(data.trigger_event_id, Some(55));
        assert_eq!(data.current_step, Some(2));
        assert_eq!(data.total_steps, Some(4));
        assert!(data.plan.is_some());
        assert!(data.completed_at.is_none());
        assert_eq!(data.created_at, Some(1699999990000));
    }

    #[test]
    fn account_info_with_all_optional_fields() {
        let json = json!({
            "account_id": "acc-full",
            "service": "openai",
            "name": "Work Account",
            "percent_remaining": 42.5,
            "reset_at": "2025-02-01T00:00:00Z",
            "tokens_used": 100000,
            "tokens_remaining": 400000,
            "tokens_limit": 500000,
            "last_refreshed_at": 1700000000000i64,
            "last_used_at": 1700000005000i64
        });
        let info: AccountInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info.name.as_deref(), Some("Work Account"));
        assert_eq!(info.tokens_used, Some(100000));
        assert_eq!(info.tokens_remaining, Some(400000));
        assert_eq!(info.tokens_limit, Some(500000));
        assert_eq!(info.reset_at.as_deref(), Some("2025-02-01T00:00:00Z"));
        assert_eq!(info.last_used_at, Some(1700000005000));
    }

    #[test]
    fn rules_lint_with_fixture_coverage() {
        let json = json!({
            "ok": true,
            "data": {
                "total_rules": 10,
                "rules_checked": 10,
                "errors": [{"rule_id": "a.b", "category": "error", "message": "bad regex", "suggestion": "fix it"}],
                "warnings": [],
                "fixture_coverage": {
                    "rules_with_fixtures": 8,
                    "rules_without_fixtures": ["c.d", "e.f"],
                    "total_fixtures": 24
                },
                "passed": false
            },
            "elapsed_ms": 10,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<RulesLintData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(!data.passed);
        assert_eq!(data.errors.len(), 1);
        assert_eq!(data.errors[0].suggestion.as_deref(), Some("fix it"));
        let fc = data.fixture_coverage.unwrap();
        assert_eq!(fc.rules_with_fixtures, 8);
        assert_eq!(fc.rules_without_fixtures, vec!["c.d", "e.f"]);
        assert_eq!(fc.total_fixtures, 24);
    }

    #[test]
    fn parse_response_typed_convenience() {
        let raw = r#"{"ok":true,"data":{"pane_id":1,"pattern":"ok","matched":false,"elapsed_ms":100,"polls":5},"elapsed_ms":110,"version":"0.1.0","now":0}"#;
        let resp: RobotResponse<WaitForData> = parse_response(raw).unwrap();
        let data = resp.into_result().unwrap();
        assert!(!data.matched);
        assert_eq!(data.polls, 5);
        assert!(!data.is_regex);
    }

    #[test]
    fn events_data_with_all_filters_populated() {
        let json = json!({
            "ok": true,
            "data": {
                "events": [],
                "total_count": 0,
                "limit": 100,
                "pane_filter": 7,
                "rule_id_filter": "codex.panic",
                "event_type_filter": "error",
                "triage_state_filter": "open",
                "label_filter": "critical",
                "unhandled_only": true,
                "since_filter": 1699000000000i64
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<EventsData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.pane_filter, Some(7));
        assert_eq!(data.rule_id_filter.as_deref(), Some("codex.panic"));
        assert_eq!(data.event_type_filter.as_deref(), Some("error"));
        assert_eq!(data.triage_state_filter.as_deref(), Some("open"));
        assert_eq!(data.label_filter.as_deref(), Some("critical"));
        assert!(data.unhandled_only);
        assert_eq!(data.since_filter, Some(1699000000000));
    }

    #[test]
    fn error_code_all_variants_roundtrip_via_number() {
        // Exhaustively verify every named variant survives number() -> from_number() roundtrip.
        let variants: Vec<ErrorCode> = vec![
            ErrorCode::WeztermNotFound,
            ErrorCode::WeztermExecFailed,
            ErrorCode::PaneNotFound,
            ErrorCode::WeztermParseFailed,
            ErrorCode::WeztermConnectionRefused,
            ErrorCode::DatabaseLocked,
            ErrorCode::StorageCorruption,
            ErrorCode::FtsIndexError,
            ErrorCode::MigrationFailed,
            ErrorCode::DiskFull,
            ErrorCode::InvalidRegex,
            ErrorCode::RulePackNotFound,
            ErrorCode::PatternTimeout,
            ErrorCode::ActionDenied,
            ErrorCode::RateLimitExceeded,
            ErrorCode::ApprovalRequired,
            ErrorCode::ApprovalExpired,
            ErrorCode::WorkflowNotFound,
            ErrorCode::WorkflowStepFailed,
            ErrorCode::WorkflowTimeout,
            ErrorCode::WorkflowAlreadyRunning,
            ErrorCode::NetworkTimeout,
            ErrorCode::ConnectionRefused,
            ErrorCode::ConfigInvalid,
            ErrorCode::ConfigNotFound,
            ErrorCode::InternalError,
            ErrorCode::FeatureNotAvailable,
            ErrorCode::VersionMismatch,
        ];
        for v in &variants {
            let n = v.number();
            let reconstructed = ErrorCode::from_number(n);
            assert_eq!(*v, reconstructed, "roundtrip failed for number {}", n);
        }
    }

    #[test]
    fn send_data_with_wait_for_and_verification_error() {
        let json = json!({
            "ok": true,
            "data": {
                "pane_id": 3,
                "injection": {"status": "allowed"},
                "wait_for": {
                    "pane_id": 3,
                    "pattern": "\\$",
                    "matched": false,
                    "elapsed_ms": 10000,
                    "polls": 50,
                    "is_regex": true
                },
                "verification_error": "timed out waiting for prompt"
            },
            "elapsed_ms": 10050,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<SendData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert_eq!(data.pane_id, 3);
        let wf = data.wait_for.unwrap();
        assert!(!wf.matched);
        assert_eq!(wf.polls, 50);
        assert!(wf.is_regex);
        assert_eq!(
            data.verification_error.as_deref(),
            Some("timed out waiting for prompt")
        );
    }

    #[test]
    fn truncation_info_serialize_roundtrip() {
        let info = TruncationInfo {
            original_bytes: 50000,
            returned_bytes: 8000,
            original_lines: 1000,
            returned_lines: 100,
        };
        let serialized = serde_json::to_string(&info).unwrap();
        let deserialized: TruncationInfo = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.original_bytes, 50000);
        assert_eq!(deserialized.returned_bytes, 8000);
        assert_eq!(deserialized.original_lines, 1000);
        assert_eq!(deserialized.returned_lines, 100);
    }

    #[test]
    fn approve_data_with_dry_run_and_fingerprint() {
        let json = json!({
            "ok": true,
            "data": {
                "code": "AP-dry",
                "valid": false,
                "dry_run": true,
                "action_fingerprint": "sha256:abc123def456"
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<ApproveData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        assert!(!data.valid);
        assert_eq!(data.dry_run, Some(true));
        assert_eq!(
            data.action_fingerprint.as_deref(),
            Some("sha256:abc123def456")
        );
        // Optional fields absent
        assert!(data.created_at.is_none());
        assert!(data.pane_id.is_none());
        assert!(data.expires_at.is_none());
    }

    #[test]
    fn why_data_with_see_also() {
        let json = json!({
            "ok": true,
            "data": {
                "code": "FT-1001",
                "category": "wezterm",
                "title": "CLI not found",
                "explanation": "WezTerm CLI binary could not be located.",
                "suggestions": ["install wezterm"],
                "see_also": ["FT-1002", "FT-1005"]
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<WhyData> = serde_json::from_value(json).unwrap();
        let data = resp.into_result().unwrap();
        let see_also = data.see_also.unwrap();
        assert_eq!(see_also.len(), 2);
        assert_eq!(see_also[0], "FT-1002");
        assert_eq!(see_also[1], "FT-1005");
    }

    #[test]
    fn quota_advisory_with_warning() {
        let json = json!({
            "availability": "low",
            "low_quota_threshold_percent": 15.0,
            "selected_percent_remaining": 12.0,
            "warning": "Account approaching quota limit",
            "blocking": true
        });
        let advisory: AccountQuotaAdvisoryInfo = serde_json::from_value(json).unwrap();
        assert_eq!(advisory.availability, "low");
        assert!(advisory.blocking);
        assert_eq!(
            advisory.warning.as_deref(),
            Some("Account approaching quota limit")
        );
        assert!((advisory.selected_percent_remaining.unwrap() - 12.0).abs() < f64::EPSILON);
    }

    #[test]
    fn reservation_info_with_released_at() {
        let json = json!({
            "id": 42,
            "pane_id": 10,
            "owner_kind": "human",
            "owner_id": "user-1",
            "reason": "debugging",
            "created_at": 1700000000000i64,
            "expires_at": 1700000060000i64,
            "released_at": 1700000030000i64,
            "status": "released"
        });
        let info: ReservationInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info.status, "released");
        assert_eq!(info.released_at, Some(1700000030000));
        assert_eq!(info.owner_kind, "human");
    }

    // ========================================================================
    // Search API types (ft-dr6zv.1.6)
    // ========================================================================

    #[test]
    fn scoring_breakdown_full_roundtrip() {
        let breakdown = SearchScoringBreakdown {
            bm25_score: Some(8.42),
            matching_terms: vec!["cargo".to_string(), "build".to_string()],
            semantic_similarity: Some(0.87),
            embedder_tier: Some("fastembed".to_string()),
            rrf_rank: Some(3),
            rrf_score: Some(0.0156),
            reranker_score: Some(0.92),
            final_score: 0.92,
        };
        let json = serde_json::to_string(&breakdown).unwrap();
        let parsed: SearchScoringBreakdown = serde_json::from_str(&json).unwrap();
        assert!((parsed.bm25_score.unwrap() - 8.42).abs() < 1e-10);
        assert_eq!(parsed.matching_terms.len(), 2);
        assert!((parsed.semantic_similarity.unwrap() - 0.87).abs() < 1e-10);
        assert_eq!(parsed.embedder_tier.as_deref(), Some("fastembed"));
        assert_eq!(parsed.rrf_rank, Some(3));
        assert!((parsed.reranker_score.unwrap() - 0.92).abs() < 1e-10);
    }

    #[test]
    fn scoring_breakdown_minimal() {
        let json = json!({ "final_score": 5.0 });
        let parsed: SearchScoringBreakdown = serde_json::from_value(json).unwrap();
        assert!((parsed.final_score - 5.0).abs() < 1e-10);
        assert!(parsed.bm25_score.is_none());
        assert!(parsed.matching_terms.is_empty());
        assert!(parsed.semantic_similarity.is_none());
        assert!(parsed.reranker_score.is_none());
    }

    #[test]
    fn explained_search_hit_flattens_base_hit() {
        let json = json!({
            "segment_id": 100,
            "pane_id": 5,
            "seq": 42,
            "captured_at": 1700000000000i64,
            "score": 0.95,
            "snippet": "$ >>cargo build<<",
            "scoring": {
                "bm25_score": 7.5,
                "matching_terms": ["cargo"],
                "final_score": 0.95
            }
        });
        let hit: ExplainedSearchHit = serde_json::from_value(json).unwrap();
        assert_eq!(hit.hit.segment_id, 100);
        assert_eq!(hit.hit.pane_id, 5);
        assert_eq!(hit.hit.snippet.as_deref(), Some("$ >>cargo build<<"));
        assert!((hit.scoring.bm25_score.unwrap() - 7.5).abs() < 1e-10);
        assert_eq!(hit.scoring.matching_terms, vec!["cargo"]);
    }

    #[test]
    fn search_explain_data_roundtrip() {
        let data = SearchExplainData {
            query: "compile error".to_string(),
            results: vec![],
            total_hits: 0,
            limit: 20,
            pane_filter: Some(3),
            mode: "hybrid".to_string(),
            timing: Some(SearchPipelineTiming {
                total_us: 1500,
                lexical_us: Some(400),
                semantic_us: Some(800),
                fusion_us: Some(200),
                rerank_us: Some(100),
            }),
            tier_metrics: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let parsed: SearchExplainData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.query, "compile error");
        assert_eq!(parsed.pane_filter, Some(3));
        assert_eq!(parsed.mode, "hybrid");
        let timing = parsed.timing.unwrap();
        assert_eq!(timing.total_us, 1500);
        assert_eq!(timing.lexical_us, Some(400));
        assert_eq!(timing.semantic_us, Some(800));
    }

    #[test]
    fn search_stream_phase_tags() {
        let fast = SearchStreamPhase::Fast { result_count: 10 };
        let json = serde_json::to_string(&fast).unwrap();
        assert!(json.contains("\"type\":\"phase_fast\""));
        let parsed: SearchStreamPhase = serde_json::from_str(&json).unwrap();
        match parsed {
            SearchStreamPhase::Fast { result_count } => assert_eq!(result_count, 10),
            _ => panic!("expected Fast variant"),
        }

        let quality = SearchStreamPhase::Quality { result_count: 5 };
        let json = serde_json::to_string(&quality).unwrap();
        assert!(json.contains("\"type\":\"phase_quality\""));

        let done = SearchStreamPhase::Done {
            total_results: 15,
            total_us: 2500,
        };
        let json = serde_json::to_string(&done).unwrap();
        assert!(json.contains("\"type\":\"phase_done\""));
        let parsed: SearchStreamPhase = serde_json::from_str(&json).unwrap();
        match parsed {
            SearchStreamPhase::Done {
                total_results,
                total_us,
            } => {
                assert_eq!(total_results, 15);
                assert_eq!(total_us, 2500);
            }
            _ => panic!("expected Done variant"),
        }
    }

    #[test]
    fn pipeline_status_data_roundtrip() {
        let status = SearchPipelineStatusData {
            state: "running".to_string(),
            watermarks: vec![
                PipelineWatermarkInfo {
                    pane_id: 1,
                    last_indexed_at_ms: 1700000000000,
                    total_docs_indexed: 42,
                    session_id: Some("sess-a".to_string()),
                },
                PipelineWatermarkInfo {
                    pane_id: 2,
                    last_indexed_at_ms: 1700000005000,
                    total_docs_indexed: 18,
                    session_id: None,
                },
            ],
            total_ticks: 100,
            total_docs_indexed: 60,
            total_lines_consumed: 3000,
            index_stats: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: SearchPipelineStatusData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.state, "running");
        assert_eq!(parsed.watermarks.len(), 2);
        assert_eq!(parsed.watermarks[0].pane_id, 1);
        assert_eq!(parsed.watermarks[0].total_docs_indexed, 42);
        assert_eq!(
            parsed.watermarks[0].session_id.as_deref(),
            Some("sess-a")
        );
        assert_eq!(parsed.total_ticks, 100);
        assert_eq!(parsed.total_docs_indexed, 60);
    }

    #[test]
    fn pipeline_control_result_roundtrip() {
        let result = SearchPipelineControlResult {
            action: "pause".to_string(),
            success: true,
            state_after: "paused".to_string(),
            message: Some("Pipeline paused successfully".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SearchPipelineControlResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.action, "pause");
        assert!(parsed.success);
        assert_eq!(parsed.state_after, "paused");
        assert_eq!(
            parsed.message.as_deref(),
            Some("Pipeline paused successfully")
        );
    }

    #[test]
    fn pipeline_control_result_minimal() {
        let json = json!({
            "action": "resume",
            "success": true,
            "state_after": "running"
        });
        let parsed: SearchPipelineControlResult = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.action, "resume");
        assert!(parsed.message.is_none());
    }

    #[test]
    fn search_explain_in_robot_envelope() {
        let json = json!({
            "ok": true,
            "data": {
                "query": "error compiling",
                "results": [{
                    "segment_id": 1,
                    "pane_id": 5,
                    "seq": 10,
                    "captured_at": 1700000000000i64,
                    "score": 0.88,
                    "scoring": {
                        "bm25_score": 6.2,
                        "matching_terms": ["error", "compiling"],
                        "semantic_similarity": 0.85,
                        "embedder_tier": "model2vec",
                        "rrf_rank": 1,
                        "rrf_score": 0.0167,
                        "final_score": 0.88
                    }
                }],
                "total_hits": 1,
                "limit": 20,
                "mode": "hybrid"
            },
            "elapsed_ms": 42,
            "version": "0.1.0",
            "now": 1700000000000u64
        });
        let resp: RobotResponse<SearchExplainData> = serde_json::from_value(json).unwrap();
        assert!(resp.ok);
        let data = resp.into_result().unwrap();
        assert_eq!(data.results.len(), 1);
        let hit = &data.results[0];
        assert_eq!(hit.hit.pane_id, 5);
        assert_eq!(hit.scoring.matching_terms.len(), 2);
        assert_eq!(hit.scoring.embedder_tier.as_deref(), Some("model2vec"));
    }

    #[test]
    fn pipeline_status_in_robot_envelope() {
        let json = json!({
            "ok": true,
            "data": {
                "state": "paused",
                "watermarks": [],
                "total_ticks": 0,
                "total_docs_indexed": 0,
                "total_lines_consumed": 0
            },
            "elapsed_ms": 1,
            "version": "0.1.0",
            "now": 0
        });
        let resp: RobotResponse<SearchPipelineStatusData> =
            serde_json::from_value(json).unwrap();
        assert!(resp.ok);
        let data = resp.into_result().unwrap();
        assert_eq!(data.state, "paused");
        assert!(data.watermarks.is_empty());
    }

    // --- Agent Inventory ---

    #[test]
    fn installed_agent_info_serde_roundtrip() {
        let info = InstalledAgentInfo {
            slug: "claude".to_string(),
            display_name: Some("Claude Code".to_string()),
            detected: true,
            evidence: vec!["default root exists: ~/.claude".to_string()],
            root_paths: vec!["/home/user/.claude".to_string()],
            config_path: Some("/home/user/.claude/config.json".to_string()),
            binary_path: Some("/usr/local/bin/claude".to_string()),
            version: Some("1.2.3".to_string()),
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: InstalledAgentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.slug, "claude");
        assert_eq!(parsed.display_name.as_deref(), Some("Claude Code"));
        assert!(parsed.detected);
        assert_eq!(parsed.evidence.len(), 1);
        assert_eq!(parsed.root_paths.len(), 1);
        assert_eq!(
            parsed.config_path.as_deref(),
            Some("/home/user/.claude/config.json")
        );
        assert_eq!(
            parsed.binary_path.as_deref(),
            Some("/usr/local/bin/claude")
        );
        assert_eq!(parsed.version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn installed_agent_info_skips_empty_fields() {
        let info = InstalledAgentInfo {
            slug: "codex".to_string(),
            display_name: None,
            detected: false,
            evidence: vec![],
            root_paths: vec![],
            config_path: None,
            binary_path: None,
            version: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("display_name"));
        assert!(!json.contains("evidence"));
        assert!(!json.contains("root_paths"));
        assert!(!json.contains("config_path"));
        assert!(!json.contains("binary_path"));
        assert!(!json.contains("version"));
    }

    #[test]
    fn running_agent_info_serde_roundtrip() {
        let info = RunningAgentInfo {
            slug: "gemini".to_string(),
            display_name: Some("Gemini".to_string()),
            state: "working".to_string(),
            session_id: Some("sess-123".to_string()),
            source: "pattern_engine".to_string(),
            pane_id: 42,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: RunningAgentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.slug, "gemini");
        assert_eq!(parsed.state, "working");
        assert_eq!(parsed.session_id.as_deref(), Some("sess-123"));
        assert_eq!(parsed.source, "pattern_engine");
        assert_eq!(parsed.pane_id, 42);
    }

    #[test]
    fn agent_inventory_summary_default() {
        let summary = AgentInventorySummary::default();
        assert_eq!(summary.installed_count, 0);
        assert_eq!(summary.running_count, 0);
        assert_eq!(summary.configured_count, 0);
        assert_eq!(summary.installed_but_idle_count, 0);
    }

    #[test]
    fn agent_inventory_data_full_roundtrip() {
        let mut running = std::collections::BTreeMap::new();
        running.insert(
            42,
            RunningAgentInfo {
                slug: "claude".to_string(),
                display_name: None,
                state: "working".to_string(),
                session_id: None,
                source: "pattern_engine".to_string(),
                pane_id: 42,
            },
        );
        let data = AgentInventoryData {
            installed: vec![InstalledAgentInfo {
                slug: "claude".to_string(),
                display_name: None,
                detected: true,
                evidence: vec!["found ~/.claude".to_string()],
                root_paths: vec![],
                config_path: None,
                binary_path: None,
                version: None,
            }],
            running,
            summary: AgentInventorySummary {
                installed_count: 1,
                running_count: 1,
                configured_count: 0,
                installed_but_idle_count: 0,
            },
            filesystem_detection_available: true,
        };
        let json = serde_json::to_string(&data).unwrap();
        let parsed: AgentInventoryData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.installed.len(), 1);
        assert_eq!(parsed.running.len(), 1);
        assert!(parsed.running.contains_key(&42));
        assert_eq!(parsed.summary.installed_count, 1);
        assert_eq!(parsed.summary.running_count, 1);
        assert!(parsed.filesystem_detection_available);
    }

    #[test]
    fn agent_inventory_data_in_robot_envelope() {
        let json = json!({
            "ok": true,
            "data": {
                "installed": [{
                    "slug": "claude",
                    "detected": true,
                    "evidence": ["found ~/.claude"]
                }],
                "running": {
                    "42": {
                        "slug": "claude",
                        "state": "working",
                        "source": "pattern_engine",
                        "pane_id": 42
                    }
                },
                "summary": {
                    "installed_count": 1,
                    "running_count": 1,
                    "configured_count": 0,
                    "installed_but_idle_count": 0
                },
                "filesystem_detection_available": true
            },
            "elapsed_ms": 5,
            "version": "0.1.0",
            "now": 1700000000000u64
        });
        let resp: RobotResponse<AgentInventoryData> = serde_json::from_value(json).unwrap();
        assert!(resp.ok);
        let data = resp.into_result().unwrap();
        assert_eq!(data.installed.len(), 1);
        assert_eq!(data.installed[0].slug, "claude");
        assert_eq!(data.running.len(), 1);
        let agent = data.running.get(&42).unwrap();
        assert_eq!(agent.state, "working");
    }

    #[test]
    fn agent_detect_refresh_result_roundtrip() {
        let result = AgentDetectRefreshResult {
            refreshed: true,
            detected_count: 3,
            total_probed: 12,
            message: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: AgentDetectRefreshResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.refreshed);
        assert_eq!(parsed.detected_count, 3);
        assert_eq!(parsed.total_probed, 12);
        assert!(parsed.message.is_none());
    }

    #[test]
    fn agent_detect_refresh_with_message() {
        let result = AgentDetectRefreshResult {
            refreshed: false,
            detected_count: 0,
            total_probed: 0,
            message: Some("agent-detection feature not available".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: AgentDetectRefreshResult = serde_json::from_str(&json).unwrap();
        assert!(!parsed.refreshed);
        assert_eq!(
            parsed.message.as_deref(),
            Some("agent-detection feature not available")
        );
    }

    #[test]
    fn from_installed_inventory_entry() {
        let entry = crate::agent_correlator::InstalledAgentInventoryEntry {
            slug: "codex".to_string(),
            detected: true,
            evidence: vec!["binary found".to_string()],
            root_paths: vec!["/home/user/.codex".to_string()],
            config_path: Some("/home/user/.codex/config.toml".to_string()),
            binary_path: None,
            version: Some("0.5.0".to_string()),
        };
        let info = InstalledAgentInfo::from(&entry);
        assert_eq!(info.slug, "codex");
        assert!(info.detected);
        assert_eq!(info.evidence.len(), 1);
        assert_eq!(info.config_path.as_deref(), Some("/home/user/.codex/config.toml"));
        assert_eq!(info.version.as_deref(), Some("0.5.0"));
    }

    #[test]
    fn from_running_inventory_entry() {
        let entry = crate::agent_correlator::RunningAgentInventoryEntry {
            slug: "claude".to_string(),
            state: "working".to_string(),
            session_id: Some("abc-123".to_string()),
            source: crate::agent_correlator::DetectionSource::PatternEngine,
        };
        let info = RunningAgentInfo::from((7u64, &entry));
        assert_eq!(info.slug, "claude");
        assert_eq!(info.state, "working");
        assert_eq!(info.session_id.as_deref(), Some("abc-123"));
        assert_eq!(info.source, "pattern_engine");
        assert_eq!(info.pane_id, 7);
    }

    #[test]
    fn from_agent_inventory() {
        let mut running = std::collections::BTreeMap::new();
        running.insert(
            10,
            crate::agent_correlator::RunningAgentInventoryEntry {
                slug: "claude".to_string(),
                state: "idle".to_string(),
                session_id: None,
                source: crate::agent_correlator::DetectionSource::PaneTitle,
            },
        );
        let inv = crate::agent_correlator::AgentInventory {
            installed: vec![
                crate::agent_correlator::InstalledAgentInventoryEntry {
                    slug: "claude".to_string(),
                    detected: true,
                    evidence: vec![],
                    root_paths: vec![],
                    config_path: Some("/p/config".to_string()),
                    binary_path: None,
                    version: None,
                },
                crate::agent_correlator::InstalledAgentInventoryEntry {
                    slug: "codex".to_string(),
                    detected: true,
                    evidence: vec![],
                    root_paths: vec![],
                    config_path: None,
                    binary_path: None,
                    version: None,
                },
            ],
            running,
        };
        let data = AgentInventoryData::from(&inv);
        assert_eq!(data.installed.len(), 2);
        assert_eq!(data.running.len(), 1);
        assert_eq!(data.summary.installed_count, 2);
        assert_eq!(data.summary.running_count, 1);
        assert_eq!(data.summary.configured_count, 1); // claude has config_path
        assert_eq!(data.summary.installed_but_idle_count, 1); // codex is installed but not running
    }

    // -----------------------------------------------------------------------
    // Mission types (ft-1i2ge.5.2)
    // -----------------------------------------------------------------------

    #[test]
    fn mission_run_state_serde_roundtrip() {
        for state in [
            MissionRunState::Pending,
            MissionRunState::Succeeded,
            MissionRunState::Failed,
            MissionRunState::Cancelled,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: MissionRunState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn mission_agent_state_serde_roundtrip() {
        for state in [
            MissionAgentState::NotRequired,
            MissionAgentState::Pending,
            MissionAgentState::Approved,
            MissionAgentState::Denied,
            MissionAgentState::Expired,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: MissionAgentState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn mission_action_state_serde_roundtrip() {
        for state in [
            MissionActionState::Ready,
            MissionActionState::Blocked,
            MissionActionState::Completed,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: MissionActionState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn mission_run_state_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&MissionRunState::Succeeded).unwrap(),
            "\"succeeded\""
        );
        assert_eq!(
            serde_json::to_string(&MissionAgentState::NotRequired).unwrap(),
            "\"not_required\""
        );
    }

    #[test]
    fn mission_assignment_counters_default() {
        let c = MissionAssignmentCounters::default();
        assert_eq!(c.pending_approval, 0);
        assert_eq!(c.approved, 0);
        assert_eq!(c.succeeded, 0);
        assert_eq!(c.failed, 0);
        assert_eq!(c.unresolved, 0);
    }

    #[test]
    fn mission_state_filters_minimal_serde() {
        let filters = MissionStateFilters {
            mission_state: None,
            run_state: None,
            agent_state: None,
            action_state: None,
            assignment_id: None,
            assignee: None,
            limit: 50,
        };
        let json = serde_json::to_string(&filters).unwrap();
        assert!(json.contains("\"limit\":50"));
        assert!(!json.contains("mission_state"));
        assert!(!json.contains("assignment_id"));
    }

    #[test]
    fn mission_state_filters_with_values() {
        let filters = MissionStateFilters {
            mission_state: Some(crate::plan::MissionLifecycleState::Running),
            run_state: Some(MissionRunState::Pending),
            agent_state: Some(MissionAgentState::Approved),
            action_state: Some(MissionActionState::Ready),
            assignment_id: Some("assign-1".to_string()),
            assignee: Some("agent-a".to_string()),
            limit: 10,
        };
        let json = serde_json::to_string(&filters).unwrap();
        let back: MissionStateFilters = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.mission_state,
            Some(crate::plan::MissionLifecycleState::Running)
        );
        assert_eq!(back.run_state, Some(MissionRunState::Pending));
        assert_eq!(back.agent_state, Some(MissionAgentState::Approved));
        assert_eq!(back.limit, 10);
    }

    #[test]
    fn mission_transition_info_serde() {
        let info = MissionTransitionInfo {
            kind: "approve".to_string(),
            to: "running".to_string(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: MissionTransitionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, "approve");
        assert_eq!(back.to, "running");
    }

    #[test]
    fn mission_assignment_data_serde_roundtrip() {
        let data = MissionAssignmentData {
            assignment_id: "a-1".to_string(),
            candidate_id: "c-1".to_string(),
            assignee: "agent-x".to_string(),
            assigned_by: crate::plan::MissionActorRole::Planner,
            action_type: "send_text".to_string(),
            run_state: MissionRunState::Pending,
            agent_state: MissionAgentState::Approved,
            action_state: MissionActionState::Ready,
            approval_state: crate::plan::ApprovalState::NotRequired,
            outcome: None,
            reason_code: None,
            error_code: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: MissionAssignmentData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.assignment_id, "a-1");
        assert_eq!(back.run_state, MissionRunState::Pending);
        assert_eq!(back.agent_state, MissionAgentState::Approved);
    }

    #[test]
    fn mission_state_data_serde_roundtrip() {
        let data = MissionStateData {
            mission_file: "mission.json".to_string(),
            mission_id: "m-1".to_string(),
            title: "Test Mission".to_string(),
            mission_hash: "abc123".to_string(),
            lifecycle_state: crate::plan::MissionLifecycleState::Running,
            mission_matches_filter: true,
            candidate_count: 5,
            assignment_count: 3,
            matched_assignment_count: 2,
            returned_assignment_count: 2,
            filters: MissionStateFilters {
                mission_state: None,
                run_state: None,
                agent_state: None,
                action_state: None,
                assignment_id: None,
                assignee: None,
                limit: 50,
            },
            assignment_counters: MissionAssignmentCounters::default(),
            available_transitions: vec![MissionTransitionInfo {
                kind: "complete".to_string(),
                to: "completed".to_string(),
            }],
            assignments: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: MissionStateData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mission_id, "m-1");
        assert_eq!(back.lifecycle_state, crate::plan::MissionLifecycleState::Running);
        assert_eq!(back.candidate_count, 5);
        assert_eq!(back.available_transitions.len(), 1);
    }

    #[test]
    fn mission_failure_catalog_entry_serde() {
        let entry = MissionFailureCatalogEntry {
            reason_code: "timeout".to_string(),
            error_code: "E-1001".to_string(),
            terminality: "non_terminal".to_string(),
            retryability: "retryable".to_string(),
            human_hint: "Retry after cooldown".to_string(),
            machine_hint: "retry_with_backoff".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: MissionFailureCatalogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reason_code, "timeout");
        assert_eq!(back.error_code, "E-1001");
    }

    #[test]
    fn mission_decision_data_serde_minimal() {
        let data = MissionDecisionData {
            assignment: MissionAssignmentData {
                assignment_id: "a-1".to_string(),
                candidate_id: "c-1".to_string(),
                assignee: "x".to_string(),
                assigned_by: crate::plan::MissionActorRole::Dispatcher,
                action_type: "send_text".to_string(),
                run_state: MissionRunState::Pending,
                agent_state: MissionAgentState::NotRequired,
                action_state: MissionActionState::Ready,
                approval_state: crate::plan::ApprovalState::NotRequired,
                outcome: None,
                reason_code: None,
                error_code: None,
            },
            candidate_action: None,
            dispatch_contract: None,
            dispatch_target: None,
            dry_run_execution: None,
            decision_error: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        // Optional fields should be omitted.
        assert!(!json.contains("candidate_action"));
        assert!(!json.contains("dispatch_contract"));
        assert!(!json.contains("decision_error"));
    }

    #[test]
    fn mission_decisions_data_serde_roundtrip() {
        let data = MissionDecisionsData {
            mission_file: "m.json".to_string(),
            mission_id: "m-1".to_string(),
            title: "Test".to_string(),
            mission_hash: "hash".to_string(),
            lifecycle_state: crate::plan::MissionLifecycleState::Planned,
            mission_matches_filter: true,
            candidate_count: 1,
            assignment_count: 1,
            matched_assignment_count: 1,
            returned_assignment_count: 1,
            filters: MissionStateFilters {
                mission_state: None,
                run_state: None,
                agent_state: None,
                action_state: None,
                assignment_id: None,
                assignee: None,
                limit: 10,
            },
            available_transitions: vec![],
            failure_catalog: vec![],
            decisions: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: MissionDecisionsData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mission_id, "m-1");
        assert_eq!(back.lifecycle_state, crate::plan::MissionLifecycleState::Planned);
    }

    #[test]
    fn mission_state_data_in_robot_envelope() {
        let data = MissionStateData {
            mission_file: "active.json".to_string(),
            mission_id: "m-test".to_string(),
            title: "Envelope Test".to_string(),
            mission_hash: "def456".to_string(),
            lifecycle_state: crate::plan::MissionLifecycleState::Completed,
            mission_matches_filter: true,
            candidate_count: 0,
            assignment_count: 0,
            matched_assignment_count: 0,
            returned_assignment_count: 0,
            filters: MissionStateFilters {
                mission_state: None,
                run_state: None,
                agent_state: None,
                action_state: None,
                assignment_id: None,
                assignee: None,
                limit: 50,
            },
            assignment_counters: MissionAssignmentCounters::default(),
            available_transitions: vec![],
            assignments: vec![],
        };
        let resp = RobotResponse::success(data, 5);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(json.contains("m-test"));
    }

    // ================================================================
    // Transactional Execution types
    // ================================================================

    #[test]
    fn tx_step_risk_serde_roundtrip() {
        for risk in [
            TxStepRisk::Low,
            TxStepRisk::Medium,
            TxStepRisk::High,
            TxStepRisk::Critical,
        ] {
            let json = serde_json::to_string(&risk).unwrap();
            let back: TxStepRisk = serde_json::from_str(&json).unwrap();
            assert_eq!(back, risk);
        }
    }

    #[test]
    fn tx_step_risk_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&TxStepRisk::Critical).unwrap(),
            "\"critical\""
        );
        assert_eq!(
            serde_json::to_string(&TxStepRisk::Low).unwrap(),
            "\"low\""
        );
    }

    #[test]
    fn tx_phase_state_serde_roundtrip() {
        for phase in [
            TxPhaseState::Planned,
            TxPhaseState::Preparing,
            TxPhaseState::Committing,
            TxPhaseState::Compensating,
            TxPhaseState::Completed,
            TxPhaseState::Aborted,
        ] {
            let json = serde_json::to_string(&phase).unwrap();
            let back: TxPhaseState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, phase);
        }
    }

    #[test]
    fn tx_precondition_kind_tagged_serde() {
        let kind = TxPreconditionKind::ReservationHeld {
            paths: vec!["src/main.rs".to_string()],
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"kind\":\"reservation_held\""));
        let back: TxPreconditionKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn tx_compensation_kind_tagged_serde() {
        let kind = TxCompensationKind::RetryWithBackoff { max_retries: 3 };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"kind\":\"retry_with_backoff\""));
        let back: TxCompensationKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn tx_risk_summary_serde() {
        let summary = TxRiskSummaryData {
            total_steps: 5,
            high_risk_count: 1,
            critical_risk_count: 0,
            uncompensated_steps: 0,
            overall_risk: TxStepRisk::Medium,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: TxRiskSummaryData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_steps, 5);
        assert_eq!(back.overall_risk, TxStepRisk::Medium);
    }

    #[test]
    fn tx_plan_data_serde_roundtrip() {
        let plan = TxPlanData {
            plan_id: "plan-001".to_string(),
            plan_hash: 0xdeadbeef,
            steps: vec![TxStepData {
                id: "s1".to_string(),
                bead_id: "b1".to_string(),
                agent_id: "agent-a".to_string(),
                description: "Deploy service".to_string(),
                depends_on: vec![],
                preconditions: vec![TxPreconditionData {
                    kind: TxPreconditionKind::PolicyApproved,
                    description: "Policy check".to_string(),
                    required: true,
                }],
                compensations: vec![TxCompensatingActionData {
                    step_id: "s1".to_string(),
                    description: "Rollback deploy".to_string(),
                    action_type: TxCompensationKind::Rollback,
                }],
                risk: TxStepRisk::Medium,
                score: 0.85,
            }],
            execution_order: vec!["s1".to_string()],
            parallel_levels: vec![vec!["s1".to_string()]],
            risk_summary: TxRiskSummaryData {
                total_steps: 1,
                high_risk_count: 0,
                critical_risk_count: 0,
                uncompensated_steps: 0,
                overall_risk: TxStepRisk::Medium,
            },
            rejected_edges: vec![],
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: TxPlanData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.plan_id, "plan-001");
        assert_eq!(back.plan_hash, 0xdeadbeef);
        assert_eq!(back.steps.len(), 1);
        assert_eq!(back.steps[0].preconditions.len(), 1);
        assert_eq!(back.steps[0].compensations.len(), 1);
    }

    #[test]
    fn tx_step_outcome_variants_serde() {
        let outcomes = vec![
            TxStepOutcome::Success {
                result: Some("ok".to_string()),
            },
            TxStepOutcome::Failed {
                error_code: "FT-5001".to_string(),
                error_message: "timeout".to_string(),
                compensated: true,
            },
            TxStepOutcome::Skipped {
                reason: "precondition unmet".to_string(),
            },
            TxStepOutcome::Compensated {
                compensation_result: "rolled back".to_string(),
            },
            TxStepOutcome::Pending,
        ];
        for outcome in &outcomes {
            let json = serde_json::to_string(outcome).unwrap();
            let back: TxStepOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, outcome);
        }
    }

    #[test]
    fn tx_resume_recommendation_serde() {
        for rec in [
            TxResumeRecommendation::ContinueFromCheckpoint,
            TxResumeRecommendation::RestartFresh,
            TxResumeRecommendation::CompensateAndAbort,
            TxResumeRecommendation::AlreadyComplete,
        ] {
            let json = serde_json::to_string(&rec).unwrap();
            let back: TxResumeRecommendation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, rec);
        }
    }

    #[test]
    fn tx_run_data_serde() {
        let data = TxRunData {
            execution_id: "exec-001".to_string(),
            plan_id: "plan-001".to_string(),
            plan_hash: 0xbeef,
            phase: TxPhaseState::Completed,
            step_count: 2,
            completed_count: 2,
            failed_count: 0,
            skipped_count: 0,
            records: vec![TxStepRecordData {
                ordinal: 0,
                step_id: "s1".to_string(),
                idem_key: "txk:abc123".to_string(),
                execution_id: "exec-001".to_string(),
                timestamp_ms: 1700000000000,
                outcome: TxStepOutcome::Success { result: None },
                risk: TxStepRisk::Low,
                prev_hash: "genesis".to_string(),
                agent_id: "agent-a".to_string(),
            }],
            chain_verification: TxChainVerificationData {
                chain_intact: true,
                first_break_at: None,
                missing_ordinals: vec![],
                total_records: 1,
            },
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxRunData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.execution_id, "exec-001");
        assert_eq!(back.phase, TxPhaseState::Completed);
        assert_eq!(back.records.len(), 1);
        assert!(back.chain_verification.chain_intact);
    }

    #[test]
    fn tx_rollback_data_serde() {
        let data = TxRollbackData {
            execution_id: "exec-002".to_string(),
            plan_id: "plan-002".to_string(),
            phase: TxPhaseState::Compensating,
            compensated_steps: vec!["s1".to_string(), "s2".to_string()],
            failed_compensations: vec![],
            total_compensated: 2,
            total_failed: 0,
            chain_verification: TxChainVerificationData {
                chain_intact: true,
                first_break_at: None,
                missing_ordinals: vec![],
                total_records: 4,
            },
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxRollbackData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.compensated_steps.len(), 2);
        assert_eq!(back.total_compensated, 2);
    }

    #[test]
    fn tx_show_data_serde() {
        let data = TxShowData {
            execution_id: "exec-003".to_string(),
            plan_id: "plan-003".to_string(),
            plan_hash: 0xcafe,
            phase: TxPhaseState::Completed,
            classification: TxBundleClassification::TeamReview,
            step_count: 3,
            record_count: 3,
            high_risk_count: 1,
            critical_risk_count: 0,
            overall_risk: TxStepRisk::High,
            chain_intact: true,
            timeline: vec![TxTimelineEntryData {
                timestamp_ms: 1700000000000,
                phase: "commit".to_string(),
                step_id: "s1".to_string(),
                kind: "step_committed".to_string(),
                reason_code: "ok".to_string(),
                summary: "Step s1 committed".to_string(),
                agent_id: "agent-a".to_string(),
                ordinal: Some(0),
                record_hash: "h123".to_string(),
            }],
            resume: None,
            records: vec![],
            redacted_field_count: 0,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxShowData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.execution_id, "exec-003");
        assert_eq!(back.classification, TxBundleClassification::TeamReview);
        assert_eq!(back.timeline.len(), 1);
    }

    #[test]
    fn tx_show_data_in_robot_envelope() {
        let data = TxShowData {
            execution_id: "exec-env".to_string(),
            plan_id: "plan-env".to_string(),
            plan_hash: 42,
            phase: TxPhaseState::Completed,
            classification: TxBundleClassification::Internal,
            step_count: 1,
            record_count: 1,
            high_risk_count: 0,
            critical_risk_count: 0,
            overall_risk: TxStepRisk::Low,
            chain_intact: true,
            timeline: vec![],
            resume: None,
            records: vec![],
            redacted_field_count: 0,
        };
        let resp = RobotResponse::success(data, 3);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(json.contains("exec-env"));
        let back: RobotResponse<TxShowData> = serde_json::from_str(&json).unwrap();
        assert!(back.ok);
        assert_eq!(back.data.unwrap().execution_id, "exec-env");
    }

    #[test]
    fn tx_chain_verification_omits_empty_fields() {
        let cv = TxChainVerificationData {
            chain_intact: true,
            first_break_at: None,
            missing_ordinals: vec![],
            total_records: 10,
        };
        let json = serde_json::to_string(&cv).unwrap();
        assert!(!json.contains("first_break_at"));
        assert!(!json.contains("missing_ordinals"));
    }

    #[test]
    fn tx_resume_data_serde() {
        let data = TxResumeData {
            execution_id: "exec-resume".to_string(),
            plan_id: "plan-resume".to_string(),
            interrupted_phase: TxPhaseState::Committing,
            completed_steps: vec!["s1".to_string()],
            failed_steps: vec!["s2".to_string()],
            remaining_steps: vec!["s3".to_string()],
            compensated_steps: vec![],
            chain_intact: true,
            last_hash: "h456".to_string(),
            recommendation: TxResumeRecommendation::ContinueFromCheckpoint,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxResumeData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.interrupted_phase, TxPhaseState::Committing);
        assert_eq!(
            back.recommendation,
            TxResumeRecommendation::ContinueFromCheckpoint
        );
    }

    #[test]
    fn tx_bundle_classification_serde() {
        for cls in [
            TxBundleClassification::Internal,
            TxBundleClassification::TeamReview,
            TxBundleClassification::ExternalAudit,
        ] {
            let json = serde_json::to_string(&cls).unwrap();
            let back: TxBundleClassification = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cls);
        }
    }
}
