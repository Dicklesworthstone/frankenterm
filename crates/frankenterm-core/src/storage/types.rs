//! Storage row-shape types extracted from `storage.rs`
//! (br-ft-bbhwz / ft-dn2tu Phase 2.3).
//!
//! This is the destination for the bulk of `storage.rs`'s
//! pub-struct surface. The full move is large (~1,440 lines per
//! the bead's audit). This first slice ships a subset of pure
//! data types whose impls have no private-helper deps:
//! `Segment`, `CheckpointResult`, `DatabasePageStats` (+ impl),
//! `Gap`, `FtsSyncConfig` (+ Default).
//!
//! Each type is re-exported from `storage.rs` so external
//! callers see no API change.
//!
//! Cross-references:
//! - parent: ft-dn2tu storage.rs split
//! - prior: ft-6qkx1 (Phase 2 schema_ddl extraction at 412d277f2)
//! - sibling: ft-94ito (Phase 2.2 — migrations_types.rs)
//! - follow-up: cont-bead for the remaining ~25 types

use serde::{Deserialize, Serialize};

/// A captured segment of pane output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    /// Unique segment ID.
    pub id: i64,
    /// Pane this segment belongs to.
    pub pane_id: u64,
    /// Sequence number within the pane (monotonically increasing).
    pub seq: u64,
    /// The captured text content.
    pub content: String,
    /// Content length (cached).
    pub content_len: usize,
    /// Optional content hash for overlap detection.
    pub content_hash: Option<String>,
    /// Timestamp when captured (epoch ms).
    pub captured_at: i64,
}

/// Result of a WAL checkpoint operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointResult {
    /// Number of WAL frames checkpointed.
    pub wal_pages: i64,
    /// Whether `PRAGMA optimize` was also run.
    pub optimized: bool,
}

/// SQLite page-level space usage, used for vacuum decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabasePageStats {
    /// Total number of pages in the database file.
    pub page_count: i64,
    /// Number of free pages currently on the freelist.
    pub free_pages: i64,
}

impl DatabasePageStats {
    /// Ratio of free pages to total pages, bounded to `[0.0, 1.0]`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn free_ratio(&self) -> f64 {
        if self.page_count <= 0 || self.free_pages <= 0 {
            return 0.0;
        }
        let bounded_free = self.free_pages.min(self.page_count);
        bounded_free as f64 / self.page_count as f64
    }
}

/// A gap event indicating discontinuous capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gap {
    /// Unique gap ID.
    pub id: i64,
    /// Pane where gap occurred.
    pub pane_id: u64,
    /// Sequence number before gap.
    pub seq_before: u64,
    /// Sequence number after gap.
    pub seq_after: u64,
    /// Reason for gap.
    pub reason: String,
    /// Timestamp of gap detection (epoch ms).
    pub detected_at: i64,
}

/// Configuration for FTS sync batching.
#[derive(Debug, Clone)]
pub struct FtsSyncConfig {
    /// Maximum segments per batch.
    pub batch_size: usize,
    /// Maximum bytes per batch.
    pub max_batch_bytes: usize,
    /// Whether to commit progress after each batch.
    pub commit_progress: bool,
}

impl Default for FtsSyncConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            max_batch_bytes: 1_048_576, // 1 MB
            commit_progress: true,
        }
    }
}

// ── ft-8bvg0 slice 2: search/index result types ─────────────────────

/// Result of an FTS search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Matching segment.
    pub segment: Segment,
    /// Snippet with highlighted terms (optional when snippets are disabled).
    pub snippet: Option<String>,
    /// Highlighted text with matching terms marked (optional).
    pub highlight: Option<String>,
    /// BM25 relevance score (lower is more relevant).
    pub score: f64,
}

/// Semantic retrieval hit keyed by segment id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticSearchHit {
    /// Matching segment id.
    pub segment_id: i64,
    /// Similarity score in `[-1.0, 1.0]` (cosine).
    pub score: f64,
}

/// Hybrid retrieval hit with explainable ranking metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridSearchResult {
    /// Segment result payload.
    pub result: SearchResult,
    /// Similarity score from semantic retrieval, when available.
    #[serde(default)]
    pub semantic_score: Option<f64>,
    /// Lexical rank position (0-based), when available.
    #[serde(default)]
    pub lexical_rank: Option<usize>,
    /// Semantic rank position (0-based), when available.
    #[serde(default)]
    pub semantic_rank: Option<usize>,
    /// Lexical lane contribution to fusion score, if applicable.
    #[serde(default)]
    pub lexical_contribution: Option<f64>,
    /// Semantic lane contribution to fusion score, if applicable.
    #[serde(default)]
    pub semantic_contribution: Option<f64>,
    /// Fused rank position (0-based).
    pub fusion_rank: usize,
    /// Fused ranking score used for ordering.
    pub fusion_score: f64,
}

/// Bundle returned by storage-level hybrid search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridSearchBundle {
    /// Search mode used for fusion.
    pub mode: String,
    /// Mode requested by the caller.
    pub requested_mode: String,
    /// Why a lexical fallback occurred (if any).
    #[serde(default)]
    pub fallback_reason: Option<String>,
    /// RRF parameter used for fusion.
    pub rrf_k: u32,
    /// Lexical lane weight used by fusion.
    pub lexical_weight: f32,
    /// Semantic lane weight used by fusion.
    pub semantic_weight: f32,
    /// Fusion backend used for ranking.
    #[serde(default)]
    pub fusion_backend: String,
    /// Number of lexical candidates considered.
    pub lexical_candidates: usize,
    /// Number of semantic candidates considered.
    pub semantic_candidates: usize,
    /// Whether semantic lane results were served from cache.
    #[serde(default)]
    pub semantic_cache_hit: bool,
    /// Semantic lane latency in milliseconds for this query.
    #[serde(default)]
    pub semantic_latency_ms: u64,
    /// Number of semantic candidate rows scanned for this query.
    #[serde(default)]
    pub semantic_rows_scanned: usize,
    /// Semantic budget state for this query (`active`, `cache_hit`, `backoff`, etc.).
    #[serde(default)]
    pub semantic_budget_state: String,
    /// Active semantic backoff deadline (epoch ms) if budget controls
    /// paused semantic execution.
    #[serde(default)]
    pub semantic_backoff_until_ms: Option<i64>,
    /// Final ranked results.
    pub results: Vec<HybridSearchResult>,
}

/// Per-pane indexing statistics for observability.
///
/// Since FTS5 indexing is trigger-driven (same transaction as
/// `INSERT`), segments and FTS rows are always in sync under
/// normal operation. A mismatch indicates index corruption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneIndexingStats {
    /// Pane ID.
    pub pane_id: u64,
    /// Total segments stored for this pane.
    pub segment_count: u64,
    /// Total content bytes stored for this pane.
    pub total_bytes: u64,
    /// Highest sequence number for this pane.
    pub max_seq: Option<u64>,
    /// Timestamp of the most recent segment (epoch ms).
    pub last_segment_at: Option<i64>,
    /// Number of FTS rows for this pane (should equal `segment_count`).
    pub fts_row_count: u64,
    /// Whether FTS index is consistent (`fts_row_count == segment_count`).
    pub fts_consistent: bool,
}

/// Aggregate indexing health across all panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingHealthReport {
    /// Per-pane statistics.
    pub panes: Vec<PaneIndexingStats>,
    /// Total segments across all panes.
    pub total_segments: u64,
    /// Total bytes across all panes.
    pub total_bytes: u64,
    /// Total FTS rows across all panes.
    pub total_fts_rows: u64,
    /// Number of panes with FTS inconsistency.
    pub inconsistent_panes: u64,
    /// Overall health: all panes consistent and no errors.
    pub healthy: bool,
}

/// Statistics for a single embedder in the embeddings table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingStats {
    /// Embedder identifier.
    pub embedder_id: String,
    /// Vector dimension.
    pub dimension: i32,
    /// Number of embedded segments.
    pub count: i64,
    /// Earliest embedding timestamp (epoch seconds).
    pub earliest_at: i64,
    /// Latest embedding timestamp (epoch seconds).
    pub latest_at: i64,
}

/// FTS index state for incremental sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsIndexState {
    /// Index version (incremented on schema changes requiring rebuild).
    pub index_version: u32,
    /// Timestamp of last full rebuild (epoch ms).
    pub last_full_rebuild_at: Option<i64>,
    /// Created timestamp (epoch ms).
    pub created_at: i64,
    /// Updated timestamp (epoch ms).
    pub updated_at: i64,
}

/// Per-pane FTS indexing progress for batched rebuild.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsPaneProgress {
    /// Pane ID.
    pub pane_id: u64,
    /// Last indexed segment sequence number.
    pub last_indexed_seq: u64,
    /// Total segments indexed for this pane.
    pub indexed_count: u64,
    /// Timestamp of last indexing (epoch ms).
    pub last_indexed_at: i64,
}

/// Result of an incremental FTS sync operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsSyncResult {
    /// Number of segments indexed in this sync.
    pub segments_indexed: u64,
    /// Number of panes processed.
    pub panes_processed: u64,
    /// Whether a full rebuild was required.
    pub full_rebuild: bool,
    /// Duration of sync in milliseconds.
    pub duration_ms: u64,
    /// Any errors encountered (non-fatal).
    pub warnings: Vec<String>,
}

// ── ft-8bvg0 slice 3: Section 3 finish (pane / event / session) ─────

/// Pane metadata and observation state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRecord {
    /// Pane ID (from WezTerm).
    pub pane_id: u64,
    /// Stable pane UUID (persists across renames/moves).
    pub pane_uuid: Option<String>,
    /// Domain name.
    pub domain: String,
    /// Window ID.
    pub window_id: Option<u64>,
    /// Tab ID.
    pub tab_id: Option<u64>,
    /// Pane title.
    pub title: Option<String>,
    /// Current working directory.
    pub cwd: Option<String>,
    /// TTY name.
    pub tty_name: Option<String>,
    /// First seen timestamp (epoch ms).
    pub first_seen_at: i64,
    /// Last seen timestamp (epoch ms).
    pub last_seen_at: i64,
    /// Whether to observe this pane.
    pub observed: bool,
    /// Reason for ignoring (if not observed).
    pub ignore_reason: Option<String>,
    /// When observation decision was made (epoch ms).
    pub last_decision_at: Option<i64>,
}

/// A stored event (pattern detection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    /// Event ID.
    pub id: i64,
    /// Pane ID.
    pub pane_id: u64,
    /// Rule ID.
    pub rule_id: String,
    /// Agent type.
    pub agent_type: String,
    /// Event type.
    pub event_type: String,
    /// Severity.
    pub severity: String,
    /// Confidence score.
    pub confidence: f64,
    /// Extracted data (JSON).
    pub extracted: Option<serde_json::Value>,
    /// Original matched text.
    pub matched_text: Option<String>,
    /// Source segment ID.
    pub segment_id: Option<i64>,
    /// Detection timestamp (epoch ms).
    pub detected_at: i64,
    /// Dedupe/identity key for repeated events.
    pub dedupe_key: Option<String>,
    /// When handled (epoch ms, `None` = unhandled).
    pub handled_at: Option<i64>,
    /// Workflow that handled this.
    pub handled_by_workflow_id: Option<String>,
    /// Handling status.
    pub handled_status: Option<String>,
}

/// Stored annotations for an event (bd-1yk8).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventAnnotations {
    /// Current triage state, if set.
    pub triage_state: Option<String>,
    /// When triage state last changed (epoch ms).
    pub triage_updated_at: Option<i64>,
    /// Who changed triage state last (optional).
    pub triage_updated_by: Option<String>,
    /// Free-form operator note (redacted at write time).
    pub note: Option<String>,
    /// When note was last updated (epoch ms).
    pub note_updated_at: Option<i64>,
    /// Who updated the note last (optional).
    pub note_updated_by: Option<String>,
    /// Labels attached to the event (sorted).
    pub labels: Vec<String>,
}

/// Persistent mute record for event identity keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMuteRecord {
    /// Identity key (hashed).
    pub identity_key: String,
    /// Scope of mute (workspace/global).
    pub scope: String,
    /// Creation timestamp (epoch ms).
    pub created_at: i64,
    /// Optional expiry timestamp (epoch ms).
    pub expires_at: Option<i64>,
    /// Optional actor identifier.
    pub created_by: Option<String>,
    /// Optional reason.
    pub reason: Option<String>,
}

/// Agent session record for tracking agent timeline and token usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionRecord {
    /// Session ID (auto-assigned).
    pub id: i64,
    /// Pane ID.
    pub pane_id: u64,
    /// Agent type (codex, claude_code, gemini, unknown).
    pub agent_type: String,
    /// Agent's internal session ID if available.
    pub session_id: Option<String>,
    /// External correlation ID (e.g., cass session).
    pub external_id: Option<String>,
    /// External correlation metadata (JSON).
    pub external_meta: Option<serde_json::Value>,
    /// Session start timestamp (epoch ms).
    pub started_at: i64,
    /// Session end timestamp (epoch ms, `None` = active).
    pub ended_at: Option<i64>,
    /// End reason (completed, limit_reached, error, manual).
    pub end_reason: Option<String>,
    /// Total tokens used.
    pub total_tokens: Option<i64>,
    /// Input tokens.
    pub input_tokens: Option<i64>,
    /// Output tokens.
    pub output_tokens: Option<i64>,
    /// Cached tokens.
    pub cached_tokens: Option<i64>,
    /// Reasoning tokens (for models that expose this).
    pub reasoning_tokens: Option<i64>,
    /// Model name.
    pub model_name: Option<String>,
    /// Estimated cost in USD.
    pub estimated_cost_usd: Option<f64>,
}

impl AgentSessionRecord {
    /// Create a new session record for starting a session.
    /// Per ft-8bvg0 slice 3: depends on `super::now_ms` from
    /// storage.rs (which is `pub fn`, so the dep stays clean).
    #[must_use]
    pub fn new_start(pane_id: u64, agent_type: &str) -> Self {
        Self {
            id: 0, // Will be assigned by DB
            pane_id,
            agent_type: agent_type.to_string(),
            session_id: None,
            external_id: None,
            external_meta: None,
            started_at: super::now_ms(),
            ended_at: None,
            end_reason: None,
            total_tokens: None,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            reasoning_tokens: None,
            model_name: None,
            estimated_cost_usd: None,
        }
    }
}

// ── ft-8bvg0 slice 4: Timeline data model (Section 4 first half) ────

/// Type of correlation between events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CorrelationType {
    /// Usage limit event followed by new session (failover).
    Failover,
    /// One event triggers another in cascade.
    Cascade,
    /// Events close in time (within window).
    Temporal,
    /// Events from the same workflow run.
    WorkflowGroup,
    /// Events with same dedupe key pattern.
    DedupeGroup,
}

impl std::fmt::Display for CorrelationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failover => write!(f, "failover"),
            Self::Cascade => write!(f, "cascade"),
            Self::Temporal => write!(f, "temporal"),
            Self::WorkflowGroup => write!(f, "workflow_group"),
            Self::DedupeGroup => write!(f, "dedupe_group"),
        }
    }
}

/// A correlation between multiple events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Correlation {
    /// Unique correlation ID.
    pub id: String,
    /// IDs of correlated events.
    pub event_ids: Vec<i64>,
    /// Type of correlation.
    pub correlation_type: CorrelationType,
    /// Confidence score (0.0–1.0).
    pub confidence: f64,
    /// Human-readable description.
    pub description: String,
}

/// Reference to a correlation (lightweight).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationRef {
    /// Correlation ID.
    pub id: String,
    /// Correlation type.
    pub correlation_type: CorrelationType,
}

/// Pane information snapshot for timeline events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Pane ID.
    pub pane_id: u64,
    /// Stable pane UUID.
    pub pane_uuid: Option<String>,
    /// Agent type detected in pane.
    pub agent_type: Option<String>,
    /// Domain (local, ssh, etc.).
    pub domain: String,
    /// Current working directory.
    pub cwd: Option<String>,
    /// Pane title.
    pub title: Option<String>,
}

/// Information about how an event was handled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandledInfo {
    /// When handled (epoch ms).
    pub handled_at: i64,
    /// Workflow that handled this.
    pub workflow_id: Option<String>,
    /// Handling status.
    pub status: String,
}

/// An event enriched with pane info and correlations for
/// timeline display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// Event ID.
    pub id: i64,
    /// Detection timestamp (epoch ms).
    pub timestamp: i64,
    /// Pane information.
    pub pane_info: PaneInfo,
    /// Rule that triggered this event.
    pub rule_id: String,
    /// Event type.
    pub event_type: String,
    /// Severity level.
    pub severity: String,
    /// Confidence score.
    pub confidence: f64,
    /// Handling information (if handled).
    pub handled: Option<HandledInfo>,
    /// References to correlations involving this event.
    pub correlations: Vec<CorrelationRef>,
    /// Brief summary for display.
    pub summary: Option<String>,
}

/// A timeline of events across panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    /// Start of time range (epoch ms).
    pub start: i64,
    /// End of time range (epoch ms).
    pub end: i64,
    /// Events in chronological order.
    pub events: Vec<TimelineEvent>,
    /// All correlations referenced by events.
    pub correlations: Vec<Correlation>,
    /// Total event count (may be more than `events.len()` if paginated).
    pub total_count: u64,
    /// Whether there are more events beyond this page.
    pub has_more: bool,
}

/// Query parameters for timeline.
#[derive(Debug, Clone, Default)]
pub struct TimelineQuery {
    /// Start of time range (epoch ms, inclusive).
    pub start: Option<i64>,
    /// End of time range (epoch ms, inclusive).
    pub end: Option<i64>,
    /// Filter by pane IDs.
    pub pane_ids: Option<Vec<u64>>,
    /// Filter by severity levels.
    pub severities: Option<Vec<String>>,
    /// Filter by event types.
    pub event_types: Option<Vec<String>>,
    /// Filter by agent types.
    pub agent_types: Option<Vec<String>>,
    /// Only unhandled events.
    pub unhandled_only: bool,
    /// Include correlations.
    pub include_correlations: bool,
    /// Maximum events to return.
    pub limit: usize,
    /// Offset for pagination.
    pub offset: usize,
}

impl TimelineQuery {
    /// Create a new query with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: 100,
            include_correlations: true,
            ..Default::default()
        }
    }

    /// Set time range.
    #[must_use]
    pub fn with_range(mut self, start: i64, end: i64) -> Self {
        self.start = Some(start);
        self.end = Some(end);
        self
    }

    /// Filter by panes.
    #[must_use]
    pub fn with_panes(mut self, pane_ids: Vec<u64>) -> Self {
        self.pane_ids = Some(pane_ids);
        self
    }

    /// Filter by severities.
    #[must_use]
    pub fn with_severities(mut self, severities: Vec<String>) -> Self {
        self.severities = Some(severities);
        self
    }

    /// Only show unhandled events.
    #[must_use]
    pub fn unhandled_only(mut self) -> Self {
        self.unhandled_only = true;
        self
    }

    /// Set pagination.
    #[must_use]
    pub fn with_pagination(mut self, limit: usize, offset: usize) -> Self {
        self.limit = limit;
        self.offset = offset;
        self
    }
}

// ── ft-8bvg0 slice 5: workflow + audit + metrics records ────────────

/// Workflow execution record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRecord {
    /// Execution ID.
    pub id: String,
    /// Workflow name.
    pub workflow_name: String,
    /// Pane ID.
    pub pane_id: u64,
    /// Trigger event ID.
    pub trigger_event_id: Option<i64>,
    /// Current step index.
    pub current_step: usize,
    /// Status.
    pub status: String,
    /// Wait condition (JSON).
    pub wait_condition: Option<serde_json::Value>,
    /// Workflow context (JSON).
    pub context: Option<serde_json::Value>,
    /// Result (JSON).
    pub result: Option<serde_json::Value>,
    /// Error message.
    pub error: Option<String>,
    /// Started timestamp (epoch ms).
    pub started_at: i64,
    /// Updated timestamp (epoch ms).
    pub updated_at: i64,
    /// Completed timestamp (epoch ms).
    pub completed_at: Option<i64>,
}

/// Workflow action plan record (canonical JSON + hash).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowActionPlanRecord {
    /// Workflow execution ID (foreign key).
    pub workflow_id: String,
    /// Content-addressed plan ID.
    pub plan_id: String,
    /// Plan hash (sha256 prefix).
    pub plan_hash: String,
    /// Canonical JSON representation of the plan.
    pub plan_json: String,
    /// Creation timestamp (epoch ms).
    pub created_at: i64,
}

/// Prepared action plan record (plan preview awaiting commit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedPlanRecord {
    /// Content-addressed plan ID.
    pub plan_id: String,
    /// Plan hash (sha256 prefix).
    pub plan_hash: String,
    /// Workspace scope for the plan.
    pub workspace_id: String,
    /// Action kind this plan represents (send_text, workflow_run, etc.).
    pub action_kind: String,
    /// Target pane ID (if applicable).
    pub pane_id: Option<u64>,
    /// Stable pane UUID (if known).
    pub pane_uuid: Option<String>,
    /// Action parameters (JSON, redacted as needed).
    pub params_json: Option<String>,
    /// Redacted plan JSON for preview.
    pub plan_json: String,
    /// Whether approval is required before commit.
    pub requires_approval: bool,
    /// Creation timestamp (epoch ms).
    pub created_at: i64,
    /// Expiration timestamp (epoch ms).
    pub expires_at: i64,
    /// When the plan was consumed (commit attempted).
    pub consumed_at: Option<i64>,
}

/// Maintenance log record for system events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceRecord {
    /// Maintenance record ID.
    pub id: i64,
    /// Event type (startup, shutdown, vacuum, retention_cleanup, error).
    pub event_type: String,
    /// Optional message.
    pub message: Option<String>,
    /// Optional JSON metadata.
    pub metadata: Option<String>,
    /// Timestamp (epoch ms).
    pub timestamp: i64,
}

/// Secret scan report record stored for incremental resumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretScanReportRecord {
    /// Report ID.
    pub id: i64,
    /// Stable hash of the scan scope (filters).
    pub scope_hash: String,
    /// JSON representation of the scan scope.
    pub scope_json: String,
    /// Report schema version.
    pub report_version: i64,
    /// Last segment ID scanned (checkpoint).
    pub last_segment_id: Option<i64>,
    /// Full report payload (JSON).
    pub report_json: String,
    /// Timestamp when the report was created (epoch ms).
    pub created_at: i64,
}

/// Type of usage metric being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricType {
    /// Tokens consumed by an API call.
    TokenUsage,
    /// Cost in USD.
    ApiCost,
    /// API call count.
    ApiCall,
    /// Rate limit event.
    RateLimitHit,
    /// Workflow execution cost.
    WorkflowCost,
    /// Session duration in seconds.
    SessionDuration,
}

impl MetricType {
    /// Convert to the SQL-stored string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            MetricType::TokenUsage => "token_usage",
            MetricType::ApiCost => "api_cost",
            MetricType::ApiCall => "api_call",
            MetricType::RateLimitHit => "rate_limit_hit",
            MetricType::WorkflowCost => "workflow_cost",
            MetricType::SessionDuration => "session_duration",
        }
    }
}

impl std::str::FromStr for MetricType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "token_usage" => Ok(MetricType::TokenUsage),
            "api_cost" => Ok(MetricType::ApiCost),
            "api_call" => Ok(MetricType::ApiCall),
            "rate_limit_hit" => Ok(MetricType::RateLimitHit),
            "workflow_cost" => Ok(MetricType::WorkflowCost),
            "session_duration" => Ok(MetricType::SessionDuration),
            _ => Err(format!("Unknown metric type: {s}")),
        }
    }
}

impl std::fmt::Display for MetricType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A usage metric record for analytics tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageMetricRecord {
    /// Record ID (0 for new records).
    pub id: i64,
    /// When the metric was recorded (epoch ms).
    pub timestamp: i64,
    /// Type of metric.
    pub metric_type: MetricType,
    /// Optional pane ID (None for global metrics).
    pub pane_id: Option<u64>,
    /// Optional agent type (codex, claude_code, gemini).
    pub agent_type: Option<String>,
    /// Optional account reference.
    pub account_id: Option<String>,
    /// Optional workflow execution reference.
    pub workflow_id: Option<String>,
    /// For countable metrics.
    pub count: Option<i64>,
    /// For costs (USD).
    pub amount: Option<f64>,
    /// For token counts.
    pub tokens: Option<i64>,
    /// Optional JSON metadata.
    pub metadata: Option<String>,
    /// When the record was created (epoch ms).
    pub created_at: i64,
}

/// Aggregated daily summary row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyMetricSummary {
    /// Day as epoch ms (midnight UTC).
    pub day_ts: i64,
    /// Agent type (None for mixed).
    pub agent_type: Option<String>,
    /// Total tokens across all metrics for the day.
    pub total_tokens: i64,
    /// Total cost in USD.
    pub total_cost: f64,
    /// Number of metric events.
    pub event_count: i64,
}

/// Per-agent metric breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMetricBreakdown {
    /// Agent type.
    pub agent_type: String,
    /// Total tokens consumed.
    pub total_tokens: i64,
    /// Total cost in USD.
    pub total_cost: f64,
    /// Average tokens per event.
    pub avg_tokens_per_event: f64,
}

/// Query filter for usage metrics.
#[derive(Debug, Clone, Default)]
pub struct MetricQuery {
    /// Filter by metric type.
    pub metric_type: Option<MetricType>,
    /// Filter by agent type.
    pub agent_type: Option<String>,
    /// Filter by account ID.
    pub account_id: Option<String>,
    /// Filter since timestamp (epoch ms).
    pub since: Option<i64>,
    /// Filter until timestamp (epoch ms).
    pub until: Option<i64>,
    /// Maximum results.
    pub limit: Option<usize>,
}

// ── ft-8bvg0 slice 6: notification + saved search + bookmark +
//                     approval token + pane reservation ─────────────

/// Status of a notification delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationStatus {
    /// Notification created, delivery not yet attempted.
    Pending,
    /// Successfully delivered.
    Sent,
    /// Delivery failed.
    Failed,
    /// Delivery was throttled / rate-limited.
    Throttled,
}

impl NotificationStatus {
    /// Convert to the SQL-stored string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            NotificationStatus::Pending => "pending",
            NotificationStatus::Sent => "sent",
            NotificationStatus::Failed => "failed",
            NotificationStatus::Throttled => "throttled",
        }
    }
}

impl std::str::FromStr for NotificationStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(NotificationStatus::Pending),
            "sent" => Ok(NotificationStatus::Sent),
            "failed" => Ok(NotificationStatus::Failed),
            "throttled" => Ok(NotificationStatus::Throttled),
            _ => Err(format!("Unknown notification status: {s}")),
        }
    }
}

impl std::fmt::Display for NotificationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A notification history record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationHistoryRecord {
    /// Record ID (0 for new records).
    pub id: i64,
    /// When the notification was created (epoch ms).
    pub timestamp: i64,
    /// Optional event ID that triggered the notification.
    pub event_id: Option<i64>,
    /// Delivery channel (webhook, desktop, slack, etc.).
    pub channel: String,
    /// Notification title.
    pub title: String,
    /// Notification body.
    pub body: String,
    /// Severity level (info, warning, error, critical).
    pub severity: String,
    /// Delivery status.
    pub status: NotificationStatus,
    /// Error message if delivery failed.
    pub error_message: Option<String>,
    /// When notification was acknowledged (epoch ms).
    pub acknowledged_at: Option<i64>,
    /// Who acknowledged the notification.
    pub acknowledged_by: Option<String>,
    /// Action taken in response.
    pub action_taken: Option<String>,
    /// Number of retry attempts.
    pub retry_count: i64,
    /// Optional JSON metadata.
    pub metadata: Option<String>,
    /// When the record was created (epoch ms).
    pub created_at: i64,
}

/// Query filter for notification history.
#[derive(Debug, Clone, Default)]
pub struct NotificationHistoryQuery {
    /// Filter since timestamp (epoch ms).
    pub since: Option<i64>,
    /// Filter until timestamp (epoch ms).
    pub until: Option<i64>,
    /// Filter by channel.
    pub channel: Option<String>,
    /// Filter by status.
    pub status: Option<NotificationStatus>,
    /// Filter by event ID.
    pub event_id: Option<i64>,
    /// Maximum results (default 100).
    pub limit: Option<usize>,
}

/// Saved search record for reusable queries and scheduling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearchRecord {
    /// Stable saved search identifier.
    pub id: String,
    /// Human-friendly name (unique).
    pub name: String,
    /// FTS query string.
    pub query: String,
    /// Optional scope to a pane.
    pub pane_id: Option<u64>,
    /// Maximum number of results.
    pub limit: i64,
    /// Since window mode (`last_run` or `fixed`).
    pub since_mode: String,
    /// Fixed since timestamp (epoch ms) when `since_mode="fixed"`.
    pub since_ms: Option<i64>,
    /// Optional schedule interval (ms). `None` means manual-only.
    pub schedule_interval_ms: Option<i64>,
    /// Whether the search is enabled for scheduling.
    pub enabled: bool,
    /// Last run timestamp (epoch ms).
    pub last_run_at: Option<i64>,
    /// Last run result count.
    pub last_result_count: Option<i64>,
    /// Last run error (if any).
    pub last_error: Option<String>,
    /// Created timestamp (epoch ms).
    pub created_at: i64,
    /// Updated timestamp (epoch ms).
    pub updated_at: i64,
}

/// A pane bookmark record binding an alias (and optional tags) to a pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneBookmarkRecord {
    pub id: i64,
    pub pane_id: u64,
    pub alias: String,
    pub tags: Option<Vec<String>>,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Default since-mode: `last_run`.
pub const SAVED_SEARCH_SINCE_MODE_LAST_RUN: &str = "last_run";
/// Fixed since-mode uses the stored `since_ms` value.
pub const SAVED_SEARCH_SINCE_MODE_FIXED: &str = "fixed";
/// Canonical value mirrored from `TuningConfig::SearchTuning`.
pub const SAVED_SEARCH_DEFAULT_LIMIT: i64 =
    crate::tuning_config::SearchTuning::DEFAULT_SAVED_SEARCH_LIMIT as i64;

impl SavedSearchRecord {
    /// Build a new saved search record with defaults. Per
    /// ft-8bvg0 slice 6: `super::now_ms` keeps the cross-module
    /// dep clean; `rand::random` for the random suffix.
    #[must_use]
    pub fn new(
        name: String,
        query: String,
        pane_id: Option<u64>,
        limit: i64,
        since_mode: String,
        since_ms: Option<i64>,
    ) -> Self {
        let now = super::now_ms();
        let random: u32 = rand::random();
        let id = format!("ss-{now}-{random:08x}");
        Self {
            id,
            name,
            query,
            pane_id,
            limit,
            since_mode,
            since_ms,
            schedule_interval_ms: None,
            enabled: false,
            last_run_at: None,
            last_result_count: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Approval token record for allow-once approvals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalTokenRecord {
    /// Token record ID.
    pub id: i64,
    /// Hash of allow-once code (sha256).
    pub code_hash: String,
    /// Created timestamp (epoch ms).
    pub created_at: i64,
    /// Expiration timestamp (epoch ms).
    pub expires_at: i64,
    /// When token was consumed (epoch ms).
    pub used_at: Option<i64>,
    /// Workspace identifier.
    pub workspace_id: String,
    /// Action kind.
    pub action_kind: String,
    /// Target pane ID (if applicable).
    pub pane_id: Option<u64>,
    /// Normalized action fingerprint.
    pub action_fingerprint: String,
    /// Optional plan hash binding (sha256 of bound `ActionPlan`).
    pub plan_hash: Option<String>,
    /// Optional plan schema version.
    pub plan_version: Option<i32>,
    /// Optional human-readable risk summary.
    pub risk_summary: Option<String>,
}

impl ApprovalTokenRecord {
    /// Returns true if the token is unused and unexpired.
    #[must_use]
    pub fn is_active(&self, now_ms: i64) -> bool {
        self.used_at.is_none() && self.expires_at >= now_ms
    }
}

/// A pane reservation representing an exclusive workflow lock on
/// a pane.
///
/// Only one active reservation per pane is allowed.
/// Reservations expire automatically after their TTL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneReservation {
    /// Unique reservation ID.
    pub id: i64,
    /// Pane this reservation applies to.
    pub pane_id: u64,
    /// Kind of owner (e.g. `workflow`, `agent`, `manual`).
    pub owner_kind: String,
    /// Owner identifier (e.g. workflow ID or agent name).
    pub owner_id: String,
    /// Human-readable reason for the reservation.
    pub reason: Option<String>,
    /// When the reservation was created (epoch ms).
    pub created_at: i64,
    /// When the reservation expires (epoch ms).
    pub expires_at: i64,
    /// When the reservation was released (epoch ms), `None` if
    /// still active.
    pub released_at: Option<i64>,
    /// Current status: `active` or `released`.
    pub status: String,
}

impl PaneReservation {
    /// Returns true if the reservation is still active and unexpired.
    #[must_use]
    pub fn is_active(&self, now_ms: i64) -> bool {
        self.status == "active" && self.released_at.is_none() && self.expires_at > now_ms
    }
}

/// Configuration for pane reservation behavior.
#[derive(Debug, Clone)]
pub struct PaneReservationConfig {
    /// Default TTL in milliseconds (30 minutes).
    pub default_ttl_ms: i64,
    /// Maximum allowed TTL in milliseconds (4 hours).
    pub max_ttl_ms: i64,
}

impl Default for PaneReservationConfig {
    fn default() -> Self {
        Self {
            default_ttl_ms: 30 * 60 * 1000, // 30 minutes
            max_ttl_ms: 4 * 60 * 60 * 1000, // 4 hours
        }
    }
}

impl PaneReservationConfig {
    /// Clamp a requested TTL to the allowed range.
    ///
    /// Returns the clamped TTL in milliseconds. The minimum is
    /// 1000 ms (1 second).
    #[must_use]
    pub fn clamp_ttl(&self, requested_ttl_ms: i64) -> i64 {
        requested_ttl_ms.clamp(1000, self.max_ttl_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// br-ft-bbhwz: free_ratio handles edge cases — empty db,
    /// freelist > total pages, both zero. Pinned at the type
    /// extraction boundary so the impl semantics survive any
    /// future refactor.
    #[test]
    fn database_page_stats_free_ratio_clamps_and_handles_zero() {
        // Both zero → 0.0.
        assert_eq!(
            DatabasePageStats {
                page_count: 0,
                free_pages: 0
            }
            .free_ratio(),
            0.0,
        );
        // Negative inputs (shouldn't happen in practice but
        // SQLite returns i64) → 0.0.
        assert_eq!(
            DatabasePageStats {
                page_count: -1,
                free_pages: 50
            }
            .free_ratio(),
            0.0,
        );
        // free > total → bounded to total → 1.0.
        assert_eq!(
            DatabasePageStats {
                page_count: 100,
                free_pages: 200
            }
            .free_ratio(),
            1.0,
        );
        // Normal case → exact ratio.
        let r = DatabasePageStats {
            page_count: 1000,
            free_pages: 250,
        }
        .free_ratio();
        assert!((r - 0.25).abs() < 1e-9);
    }

    /// br-ft-bbhwz: FtsSyncConfig::default values are stable.
    /// Operators tuning batch sizes downstream rely on the
    /// 100-segment / 1-MB / commit_progress=true defaults.
    #[test]
    fn fts_sync_config_default_is_stable() {
        let cfg = FtsSyncConfig::default();
        assert_eq!(cfg.batch_size, 100);
        assert_eq!(cfg.max_batch_bytes, 1_048_576);
        assert!(cfg.commit_progress);
    }

    /// br-ft-bbhwz: Segment serde round-trip preserves every
    /// field byte-for-byte. The wire format downstream callers
    /// depend on cannot drift across the extraction boundary.
    #[test]
    fn segment_serde_roundtrip() {
        let s = Segment {
            id: 42,
            pane_id: 7,
            seq: 1234,
            content: "hello".to_string(),
            content_len: 5,
            content_hash: Some("abc123".to_string()),
            captured_at: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Segment = serde_json::from_str(&json).unwrap();
        assert_eq!(s.id, back.id);
        assert_eq!(s.pane_id, back.pane_id);
        assert_eq!(s.seq, back.seq);
        assert_eq!(s.content, back.content);
        assert_eq!(s.content_len, back.content_len);
        assert_eq!(s.content_hash, back.content_hash);
        assert_eq!(s.captured_at, back.captured_at);
    }

    /// br-ft-bbhwz: Gap serde round-trip — same contract.
    #[test]
    fn gap_serde_roundtrip() {
        let g = Gap {
            id: 1,
            pane_id: 7,
            seq_before: 100,
            seq_after: 110,
            reason: "daemon restart".to_string(),
            detected_at: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&g).unwrap();
        let back: Gap = serde_json::from_str(&json).unwrap();
        assert_eq!(g.id, back.id);
        assert_eq!(g.pane_id, back.pane_id);
        assert_eq!(g.seq_before, back.seq_before);
        assert_eq!(g.seq_after, back.seq_after);
        assert_eq!(g.reason, back.reason);
        assert_eq!(g.detected_at, back.detected_at);
    }
}
