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
