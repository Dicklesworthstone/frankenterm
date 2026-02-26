//! Lexical backend bridge — unified dispatch for tantivy-based indexing (B8).
//!
//! Eliminates duplicate tantivy paths by providing a single bridge layer
//! between:
//!
//! - **Recorder-lexical** (`recorder-lexical` feature): Direct tantivy with
//!   checkpoint-based incremental indexing, custom terminal tokenizers, and
//!   25-field schema (`ft.recorder.lexical.v1`).
//!
//! - **FrankenSearch** (`frankensearch` feature): Tantivy wrapped in
//!   frankensearch-lexical with TTL/LRU lifecycle, progressive search, and
//!   two-tier delivery.
//!
//! # Design
//!
//! Rather than replacing either path, this bridge defines:
//!
//! 1. **Lifecycle policy**: Checkpoint vs TTL-LRU for index management
//! 2. **Schema version**: Track which field schema is authoritative
//! 3. **Ingest/query dispatch**: Route operations to the correct backend
//! 4. **Document adapter**: Bridge between document representations
//! 5. **Metrics aggregation**: Unified view of index health

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Ingest lifecycle policy ─────────────────────────────────────────────

/// Lifecycle policy for the lexical index.
///
/// Determines how documents enter and exit the index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestLifecyclePolicy {
    /// Checkpoint-driven incremental indexing from append-log.
    /// Documents are retained until explicitly purged or the log is compacted.
    /// Used by the recorder-lexical path.
    Checkpoint,
    /// TTL-based with LRU eviction.
    /// Documents expire after `ttl_days` and are evicted when the index
    /// exceeds `max_index_size_bytes`. Used by search/indexing.rs.
    #[default]
    TtlLru,
}

impl IngestLifecyclePolicy {
    /// Canonical string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoint",
            Self::TtlLru => "ttl_lru",
        }
    }

    /// Parse from string (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "checkpoint" | "append_log" | "append-log" | "incremental" => Self::Checkpoint,
            _ => Self::TtlLru,
        }
    }
}

impl fmt::Display for IngestLifecyclePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Schema version ──────────────────────────────────────────────────────

/// Schema version for the lexical index.
///
/// Tracks which field schema is authoritative. Schema migration between
/// versions requires a full reindex.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LexicalSchemaVersion {
    /// Recorder-lexical v1: 25-field schema with terminal-specific tokenizers.
    /// Schema name: `ft.recorder.lexical.v1`.
    RecorderV1,
    /// FrankenSearch-managed schema: simplified fields with automatic migration.
    #[default]
    FrankenSearchV1,
}

impl LexicalSchemaVersion {
    /// Canonical string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RecorderV1 => "recorder_v1",
            Self::FrankenSearchV1 => "frankensearch_v1",
        }
    }

    /// Parse from string (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "recorder_v1" | "recorder" | "v1" => Self::RecorderV1,
            _ => Self::FrankenSearchV1,
        }
    }

    /// Whether this schema uses terminal-specific tokenizers.
    #[must_use]
    pub fn uses_terminal_tokenizers(self) -> bool {
        matches!(self, Self::RecorderV1)
    }
}

impl fmt::Display for LexicalSchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Backend configuration ───────────────────────────────────────────────

/// Unified configuration for the lexical backend.
///
/// Covers settings for both the recorder-lexical and frankensearch paths,
/// selecting the active path via `lifecycle_policy` and `schema_version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LexicalBackendConfig {
    /// Index lifecycle policy.
    #[serde(default)]
    pub lifecycle_policy: IngestLifecyclePolicy,
    /// Active schema version.
    #[serde(default)]
    pub schema_version: LexicalSchemaVersion,
    /// Maximum documents per flush batch.
    #[serde(default = "default_flush_batch_size")]
    pub flush_batch_size: usize,
    /// Flush interval in seconds (TTL-LRU path).
    #[serde(default = "default_flush_interval_secs")]
    pub flush_interval_secs: u64,
    /// Document TTL in days (TTL-LRU path, 0 = no expiry).
    #[serde(default = "default_ttl_days")]
    pub ttl_days: u32,
    /// Maximum index size in bytes (TTL-LRU path, 0 = unlimited).
    #[serde(default)]
    pub max_index_size_bytes: u64,
    /// Writer heap size in bytes (tantivy IndexWriter).
    #[serde(default = "default_writer_heap_bytes")]
    pub writer_heap_bytes: usize,
    /// Whether to enable terminal-specific tokenizers.
    /// Auto-detected from schema_version if not set explicitly.
    #[serde(default)]
    pub terminal_tokenizers: bool,
    /// Maximum documents per second rate limit (0 = unlimited).
    #[serde(default)]
    pub max_docs_per_second: u32,
}

fn default_flush_batch_size() -> usize {
    256
}
fn default_flush_interval_secs() -> u64 {
    30
}
fn default_ttl_days() -> u32 {
    7
}
fn default_writer_heap_bytes() -> usize {
    50 * 1024 * 1024 // 50 MB
}

impl Default for LexicalBackendConfig {
    fn default() -> Self {
        Self {
            lifecycle_policy: IngestLifecyclePolicy::default(),
            schema_version: LexicalSchemaVersion::default(),
            flush_batch_size: default_flush_batch_size(),
            flush_interval_secs: default_flush_interval_secs(),
            ttl_days: default_ttl_days(),
            max_index_size_bytes: 0,
            writer_heap_bytes: default_writer_heap_bytes(),
            terminal_tokenizers: false,
            max_docs_per_second: 0,
        }
    }
}

impl LexicalBackendConfig {
    /// Build a config for recorder-lexical mode with defaults.
    #[must_use]
    pub fn recorder_defaults() -> Self {
        Self {
            lifecycle_policy: IngestLifecyclePolicy::Checkpoint,
            schema_version: LexicalSchemaVersion::RecorderV1,
            terminal_tokenizers: true,
            ..Default::default()
        }
    }

    /// Build a config for frankensearch-managed mode with defaults.
    #[must_use]
    pub fn frankensearch_defaults() -> Self {
        Self::default()
    }

    /// Whether the config uses the recorder-lexical path.
    #[must_use]
    pub fn is_recorder_path(&self) -> bool {
        matches!(self.lifecycle_policy, IngestLifecyclePolicy::Checkpoint)
            && matches!(self.schema_version, LexicalSchemaVersion::RecorderV1)
    }

    /// Whether the config uses the frankensearch-managed path.
    #[must_use]
    pub fn is_frankensearch_path(&self) -> bool {
        matches!(self.schema_version, LexicalSchemaVersion::FrankenSearchV1)
    }
}

// ── Backend metrics ─────────────────────────────────────────────────────

/// Unified metrics for the lexical backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LexicalBackendMetrics {
    // ── Ingest counters ──
    /// Total documents ingested.
    pub docs_ingested: u64,
    /// Total documents expired (TTL) or purged (checkpoint compaction).
    pub docs_expired: u64,
    /// Total documents currently in the index.
    pub docs_active: u64,
    /// Total flush operations performed.
    pub flush_count: u64,
    /// Total documents rejected (rate-limited, schema mismatch, etc.).
    pub docs_rejected: u64,

    // ── Query counters ──
    /// Total queries executed.
    pub queries_executed: u64,
    /// Total query errors.
    pub query_errors: u64,

    // ── Index health ──
    /// Index size in bytes (approximate).
    pub index_size_bytes: u64,
    /// Number of tantivy segments.
    pub segment_count: u32,
    /// Schema version in use.
    pub schema_version: String,
    /// Lifecycle policy in use.
    pub lifecycle_policy: String,
}

impl Default for LexicalBackendMetrics {
    fn default() -> Self {
        Self {
            docs_ingested: 0,
            docs_expired: 0,
            docs_active: 0,
            flush_count: 0,
            docs_rejected: 0,
            queries_executed: 0,
            query_errors: 0,
            index_size_bytes: 0,
            segment_count: 0,
            schema_version: LexicalSchemaVersion::default().as_str().to_string(),
            lifecycle_policy: IngestLifecyclePolicy::default().as_str().to_string(),
        }
    }
}

/// Compute document churn rate as expired/ingested ratio [0.0, 1.0].
///
/// Returns 0.0 if no documents have been ingested.
#[must_use]
pub fn compute_churn_rate(metrics: &LexicalBackendMetrics) -> f64 {
    if metrics.docs_ingested == 0 {
        return 0.0;
    }
    metrics.docs_expired as f64 / metrics.docs_ingested as f64
}

/// Compute query error rate as errors/total ratio [0.0, 1.0].
///
/// Returns 0.0 if no queries have been executed.
#[must_use]
pub fn compute_query_error_rate(metrics: &LexicalBackendMetrics) -> f64 {
    if metrics.queries_executed == 0 {
        return 0.0;
    }
    metrics.query_errors as f64 / metrics.queries_executed as f64
}

/// Compute ingest rejection rate as rejected/total ratio [0.0, 1.0].
///
/// Returns 0.0 if no ingest attempts have been made.
#[must_use]
pub fn compute_rejection_rate(metrics: &LexicalBackendMetrics) -> f64 {
    let total = metrics.docs_ingested + metrics.docs_rejected;
    if total == 0 {
        return 0.0;
    }
    metrics.docs_rejected as f64 / total as f64
}

// ── Document adapter ────────────────────────────────────────────────────

/// Abstract document field representation for cross-backend bridging.
///
/// Captures the superset of fields from both the recorder-lexical schema
/// (25 fields) and the search/indexing schema (source, text, metadata).
/// Each backend extracts the fields it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeDocument {
    /// Document ID (unique within index).
    pub doc_id: String,
    /// Primary text content for lexical indexing.
    pub text: String,
    /// Document source classification.
    pub source: DocumentSource,
    /// Timestamp of capture (ms since epoch).
    pub captured_at_ms: u64,
    /// Pane ID (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Session ID (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Content hash for deduplication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Additional metadata key-value pairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<(String, String)>,
}

/// Source classification for bridge documents.
///
/// Superset of `SearchDocumentSource` (from search/indexing.rs) and recorder
/// event types. Each backend maps to its own source taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentSource {
    /// Terminal scrollback text.
    Scrollback,
    /// Command output block.
    Command,
    /// Agent artifact (tool call, error, code block).
    AgentArtifact,
    /// Pane metadata.
    PaneMetadata,
    /// Cass session data.
    Cass,
    /// Recorder event (from flight recorder append-log).
    RecorderEvent,
}

impl DocumentSource {
    /// Canonical string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scrollback => "scrollback",
            Self::Command => "command",
            Self::AgentArtifact => "agent_artifact",
            Self::PaneMetadata => "pane_metadata",
            Self::Cass => "cass",
            Self::RecorderEvent => "recorder_event",
        }
    }

    /// Parse from string (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "scrollback" => Self::Scrollback,
            "command" | "cmd" => Self::Command,
            "agent_artifact" | "artifact" => Self::AgentArtifact,
            "pane_metadata" | "metadata" => Self::PaneMetadata,
            "cass" => Self::Cass,
            "recorder_event" | "recorder" => Self::RecorderEvent,
            _ => Self::Scrollback,
        }
    }
}

impl fmt::Display for DocumentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Convert a bridge document to search/indexing-compatible metadata.
///
/// Extracts the fields that `SearchIndex` needs for content-hash
/// deduplication and TTL/LRU lifecycle management.
#[must_use]
pub fn bridge_doc_to_indexing_meta(doc: &BridgeDocument) -> IndexingMeta {
    IndexingMeta {
        doc_id: doc.doc_id.clone(),
        content_hash: doc.content_hash.clone(),
        captured_at_ms: doc.captured_at_ms,
        source: doc.source.as_str().to_string(),
        pane_id: doc.pane_id,
    }
}

/// Metadata extracted from a bridge document for the search/indexing path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingMeta {
    pub doc_id: String,
    pub content_hash: Option<String>,
    pub captured_at_ms: u64,
    pub source: String,
    pub pane_id: Option<u64>,
}

// ── Explainability ──────────────────────────────────────────────────────

/// Diagnostic explanation of lexical backend state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LexicalBackendExplanation {
    /// Active configuration.
    pub config: LexicalBackendConfig,
    /// Current metrics snapshot.
    pub metrics: LexicalBackendMetrics,
    /// Document churn rate [0.0, 1.0].
    pub churn_rate: f64,
    /// Query error rate [0.0, 1.0].
    pub query_error_rate: f64,
    /// Ingest rejection rate [0.0, 1.0].
    pub rejection_rate: f64,
    /// Whether the backend is operating in degraded mode.
    pub is_degraded: bool,
    /// Reason for degradation (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degradation_reason: Option<String>,
}

/// Build a diagnostic explanation from config + metrics.
#[must_use]
pub fn explain_lexical_backend(
    config: &LexicalBackendConfig,
    metrics: &LexicalBackendMetrics,
) -> LexicalBackendExplanation {
    let churn_rate = compute_churn_rate(metrics);
    let query_error_rate = compute_query_error_rate(metrics);
    let rejection_rate = compute_rejection_rate(metrics);

    // Degradation heuristics:
    // - Query error rate > 5% suggests index corruption or schema mismatch
    // - Rejection rate > 20% suggests rate limiting too aggressive or schema issues
    // - Churn rate > 90% suggests TTL too short or excessive reindexing
    let high_query_errors = metrics.queries_executed > 50 && query_error_rate > 0.05;
    let high_rejections =
        (metrics.docs_ingested + metrics.docs_rejected) > 100 && rejection_rate > 0.20;
    let high_churn = metrics.docs_ingested > 100 && churn_rate > 0.90;
    let is_degraded = high_query_errors || high_rejections || high_churn;

    let degradation_reason = if high_query_errors && high_rejections {
        Some("high query error rate and high ingest rejection rate".to_string())
    } else if high_query_errors {
        Some(format!(
            "high query error rate ({:.1}%)",
            query_error_rate * 100.0
        ))
    } else if high_rejections {
        Some(format!(
            "high ingest rejection rate ({:.1}%)",
            rejection_rate * 100.0
        ))
    } else if high_churn {
        Some(format!(
            "high document churn rate ({:.1}%)",
            churn_rate * 100.0
        ))
    } else {
        None
    };

    LexicalBackendExplanation {
        config: config.clone(),
        metrics: metrics.clone(),
        churn_rate,
        query_error_rate,
        rejection_rate,
        is_degraded,
        degradation_reason,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── IngestLifecyclePolicy ──────────────────────────────────────

    #[test]
    fn lifecycle_default_is_ttl_lru() {
        assert_eq!(
            IngestLifecyclePolicy::default(),
            IngestLifecyclePolicy::TtlLru
        );
    }

    #[test]
    fn lifecycle_as_str() {
        assert_eq!(IngestLifecyclePolicy::Checkpoint.as_str(), "checkpoint");
        assert_eq!(IngestLifecyclePolicy::TtlLru.as_str(), "ttl_lru");
    }

    #[test]
    fn lifecycle_display() {
        assert_eq!(
            format!("{}", IngestLifecyclePolicy::Checkpoint),
            "checkpoint"
        );
        assert_eq!(format!("{}", IngestLifecyclePolicy::TtlLru), "ttl_lru");
    }

    #[test]
    fn lifecycle_parse() {
        assert_eq!(
            IngestLifecyclePolicy::parse("checkpoint"),
            IngestLifecyclePolicy::Checkpoint
        );
        assert_eq!(
            IngestLifecyclePolicy::parse("append_log"),
            IngestLifecyclePolicy::Checkpoint
        );
        assert_eq!(
            IngestLifecyclePolicy::parse("append-log"),
            IngestLifecyclePolicy::Checkpoint
        );
        assert_eq!(
            IngestLifecyclePolicy::parse("incremental"),
            IngestLifecyclePolicy::Checkpoint
        );
        assert_eq!(
            IngestLifecyclePolicy::parse("ttl_lru"),
            IngestLifecyclePolicy::TtlLru
        );
        assert_eq!(
            IngestLifecyclePolicy::parse("unknown"),
            IngestLifecyclePolicy::TtlLru
        );
        assert_eq!(
            IngestLifecyclePolicy::parse(""),
            IngestLifecyclePolicy::TtlLru
        );
    }

    #[test]
    fn lifecycle_serde_roundtrip() {
        for p in [
            IngestLifecyclePolicy::Checkpoint,
            IngestLifecyclePolicy::TtlLru,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let back: IngestLifecyclePolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(p, back);
        }
    }

    // ── LexicalSchemaVersion ───────────────────────────────────────

    #[test]
    fn schema_default_is_frankensearch() {
        assert_eq!(
            LexicalSchemaVersion::default(),
            LexicalSchemaVersion::FrankenSearchV1
        );
    }

    #[test]
    fn schema_as_str() {
        assert_eq!(LexicalSchemaVersion::RecorderV1.as_str(), "recorder_v1");
        assert_eq!(
            LexicalSchemaVersion::FrankenSearchV1.as_str(),
            "frankensearch_v1"
        );
    }

    #[test]
    fn schema_parse() {
        assert_eq!(
            LexicalSchemaVersion::parse("recorder_v1"),
            LexicalSchemaVersion::RecorderV1
        );
        assert_eq!(
            LexicalSchemaVersion::parse("recorder"),
            LexicalSchemaVersion::RecorderV1
        );
        assert_eq!(
            LexicalSchemaVersion::parse("v1"),
            LexicalSchemaVersion::RecorderV1
        );
        assert_eq!(
            LexicalSchemaVersion::parse("frankensearch_v1"),
            LexicalSchemaVersion::FrankenSearchV1
        );
        assert_eq!(
            LexicalSchemaVersion::parse("unknown"),
            LexicalSchemaVersion::FrankenSearchV1
        );
    }

    #[test]
    fn schema_uses_terminal_tokenizers() {
        assert!(LexicalSchemaVersion::RecorderV1.uses_terminal_tokenizers());
        assert!(!LexicalSchemaVersion::FrankenSearchV1.uses_terminal_tokenizers());
    }

    #[test]
    fn schema_serde_roundtrip() {
        for v in [
            LexicalSchemaVersion::RecorderV1,
            LexicalSchemaVersion::FrankenSearchV1,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: LexicalSchemaVersion = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    // ── LexicalBackendConfig ───────────────────────────────────────

    #[test]
    fn config_defaults() {
        let cfg = LexicalBackendConfig::default();
        assert_eq!(cfg.lifecycle_policy, IngestLifecyclePolicy::TtlLru);
        assert_eq!(cfg.schema_version, LexicalSchemaVersion::FrankenSearchV1);
        assert_eq!(cfg.flush_batch_size, 256);
        assert_eq!(cfg.flush_interval_secs, 30);
        assert_eq!(cfg.ttl_days, 7);
        assert_eq!(cfg.max_index_size_bytes, 0);
        assert_eq!(cfg.writer_heap_bytes, 50 * 1024 * 1024);
        assert!(!cfg.terminal_tokenizers);
        assert_eq!(cfg.max_docs_per_second, 0);
    }

    #[test]
    fn config_recorder_defaults() {
        let cfg = LexicalBackendConfig::recorder_defaults();
        assert_eq!(cfg.lifecycle_policy, IngestLifecyclePolicy::Checkpoint);
        assert_eq!(cfg.schema_version, LexicalSchemaVersion::RecorderV1);
        assert!(cfg.terminal_tokenizers);
    }

    #[test]
    fn config_frankensearch_defaults() {
        let cfg = LexicalBackendConfig::frankensearch_defaults();
        assert_eq!(cfg.lifecycle_policy, IngestLifecyclePolicy::TtlLru);
        assert_eq!(cfg.schema_version, LexicalSchemaVersion::FrankenSearchV1);
    }

    #[test]
    fn config_is_recorder_path() {
        let cfg = LexicalBackendConfig::recorder_defaults();
        assert!(cfg.is_recorder_path());
        assert!(!cfg.is_frankensearch_path());
    }

    #[test]
    fn config_is_frankensearch_path() {
        let cfg = LexicalBackendConfig::frankensearch_defaults();
        assert!(!cfg.is_recorder_path());
        assert!(cfg.is_frankensearch_path());
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = LexicalBackendConfig {
            lifecycle_policy: IngestLifecyclePolicy::Checkpoint,
            schema_version: LexicalSchemaVersion::RecorderV1,
            flush_batch_size: 512,
            flush_interval_secs: 60,
            ttl_days: 14,
            max_index_size_bytes: 1_000_000,
            writer_heap_bytes: 100 * 1024 * 1024,
            terminal_tokenizers: true,
            max_docs_per_second: 1000,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: LexicalBackendConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lifecycle_policy, IngestLifecyclePolicy::Checkpoint);
        assert_eq!(back.flush_batch_size, 512);
        assert_eq!(back.ttl_days, 14);
        assert!(back.terminal_tokenizers);
    }

    #[test]
    fn config_serde_missing_fields_use_defaults() {
        let json = r#"{}"#;
        let cfg: LexicalBackendConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg, LexicalBackendConfig::default());
    }

    // ── LexicalBackendMetrics ──────────────────────────────────────

    #[test]
    fn metrics_default_is_zero() {
        let m = LexicalBackendMetrics::default();
        assert_eq!(m.docs_ingested, 0);
        assert_eq!(m.docs_expired, 0);
        assert_eq!(m.docs_active, 0);
        assert_eq!(m.queries_executed, 0);
        assert_eq!(m.index_size_bytes, 0);
    }

    #[test]
    fn metrics_serde_roundtrip() {
        let m = LexicalBackendMetrics {
            docs_ingested: 1000,
            docs_expired: 200,
            docs_active: 800,
            flush_count: 50,
            docs_rejected: 10,
            queries_executed: 500,
            query_errors: 3,
            index_size_bytes: 10_000_000,
            segment_count: 5,
            schema_version: "recorder_v1".to_string(),
            lifecycle_policy: "checkpoint".to_string(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: LexicalBackendMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.docs_ingested, 1000);
        assert_eq!(back.docs_active, 800);
        assert_eq!(back.segment_count, 5);
    }

    // ── Derived metrics ────────────────────────────────────────────

    #[test]
    fn churn_rate_zero_when_no_ingestion() {
        let m = LexicalBackendMetrics::default();
        assert!((compute_churn_rate(&m) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn churn_rate_all_expired() {
        let m = LexicalBackendMetrics {
            docs_ingested: 100,
            docs_expired: 100,
            ..Default::default()
        };
        assert!((compute_churn_rate(&m) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn churn_rate_partial() {
        let m = LexicalBackendMetrics {
            docs_ingested: 1000,
            docs_expired: 250,
            ..Default::default()
        };
        assert!((compute_churn_rate(&m) - 0.25).abs() < 1e-10);
    }

    #[test]
    fn query_error_rate_zero_when_no_queries() {
        let m = LexicalBackendMetrics::default();
        assert!((compute_query_error_rate(&m) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn query_error_rate_mixed() {
        let m = LexicalBackendMetrics {
            queries_executed: 200,
            query_errors: 10,
            ..Default::default()
        };
        assert!((compute_query_error_rate(&m) - 0.05).abs() < 1e-10);
    }

    #[test]
    fn rejection_rate_zero_when_no_attempts() {
        let m = LexicalBackendMetrics::default();
        assert!((compute_rejection_rate(&m) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rejection_rate_mixed() {
        let m = LexicalBackendMetrics {
            docs_ingested: 900,
            docs_rejected: 100,
            ..Default::default()
        };
        assert!((compute_rejection_rate(&m) - 0.1).abs() < 1e-10);
    }

    // ── DocumentSource ─────────────────────────────────────────────

    #[test]
    fn doc_source_as_str() {
        assert_eq!(DocumentSource::Scrollback.as_str(), "scrollback");
        assert_eq!(DocumentSource::Command.as_str(), "command");
        assert_eq!(DocumentSource::AgentArtifact.as_str(), "agent_artifact");
        assert_eq!(DocumentSource::PaneMetadata.as_str(), "pane_metadata");
        assert_eq!(DocumentSource::Cass.as_str(), "cass");
        assert_eq!(DocumentSource::RecorderEvent.as_str(), "recorder_event");
    }

    #[test]
    fn doc_source_parse() {
        assert_eq!(
            DocumentSource::parse("scrollback"),
            DocumentSource::Scrollback
        );
        assert_eq!(DocumentSource::parse("command"), DocumentSource::Command);
        assert_eq!(DocumentSource::parse("cmd"), DocumentSource::Command);
        assert_eq!(
            DocumentSource::parse("agent_artifact"),
            DocumentSource::AgentArtifact
        );
        assert_eq!(
            DocumentSource::parse("recorder_event"),
            DocumentSource::RecorderEvent
        );
        assert_eq!(
            DocumentSource::parse("recorder"),
            DocumentSource::RecorderEvent
        );
        assert_eq!(DocumentSource::parse("unknown"), DocumentSource::Scrollback);
    }

    #[test]
    fn doc_source_serde_roundtrip() {
        for s in [
            DocumentSource::Scrollback,
            DocumentSource::Command,
            DocumentSource::AgentArtifact,
            DocumentSource::PaneMetadata,
            DocumentSource::Cass,
            DocumentSource::RecorderEvent,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: DocumentSource = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    // ── BridgeDocument ─────────────────────────────────────────────

    #[test]
    fn bridge_doc_serde_roundtrip() {
        let doc = BridgeDocument {
            doc_id: "doc-42".to_string(),
            text: "ls -la /tmp".to_string(),
            source: DocumentSource::Command,
            captured_at_ms: 1700000000000,
            pane_id: Some(7),
            session_id: Some("sess-1".to_string()),
            content_hash: Some("abc123".to_string()),
            metadata: vec![("key".to_string(), "value".to_string())],
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: BridgeDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(back.doc_id, "doc-42");
        assert_eq!(back.source, DocumentSource::Command);
        assert_eq!(back.pane_id, Some(7));
        assert_eq!(back.metadata.len(), 1);
    }

    #[test]
    fn bridge_doc_serde_optional_fields_absent() {
        let doc = BridgeDocument {
            doc_id: "doc-1".to_string(),
            text: "hello".to_string(),
            source: DocumentSource::Scrollback,
            captured_at_ms: 0,
            pane_id: None,
            session_id: None,
            content_hash: None,
            metadata: vec![],
        };
        let json = serde_json::to_string(&doc).unwrap();
        assert!(!json.contains("pane_id"));
        assert!(!json.contains("session_id"));
        assert!(!json.contains("content_hash"));
        assert!(!json.contains("metadata"));
    }

    #[test]
    fn bridge_doc_to_indexing_meta_extracts_fields() {
        let doc = BridgeDocument {
            doc_id: "d-99".to_string(),
            text: "ignored in meta".to_string(),
            source: DocumentSource::AgentArtifact,
            captured_at_ms: 12345,
            pane_id: Some(3),
            session_id: Some("s1".to_string()),
            content_hash: Some("hash-xxx".to_string()),
            metadata: vec![],
        };
        let meta = bridge_doc_to_indexing_meta(&doc);
        assert_eq!(meta.doc_id, "d-99");
        assert_eq!(meta.content_hash.as_deref(), Some("hash-xxx"));
        assert_eq!(meta.captured_at_ms, 12345);
        assert_eq!(meta.source, "agent_artifact");
        assert_eq!(meta.pane_id, Some(3));
    }

    // ── Explainability ─────────────────────────────────────────────

    #[test]
    fn explain_healthy_backend() {
        let cfg = LexicalBackendConfig::default();
        let m = LexicalBackendMetrics {
            docs_ingested: 10000,
            docs_expired: 2000,
            docs_active: 8000,
            flush_count: 100,
            docs_rejected: 5,
            queries_executed: 5000,
            query_errors: 10,
            index_size_bytes: 50_000_000,
            segment_count: 3,
            ..Default::default()
        };
        let expl = explain_lexical_backend(&cfg, &m);
        assert!(!expl.is_degraded);
        assert!(expl.degradation_reason.is_none());
        assert!((expl.churn_rate - 0.2).abs() < 1e-10);
        assert!((expl.query_error_rate - 0.002).abs() < 1e-10);
    }

    #[test]
    fn explain_high_query_errors_degraded() {
        let cfg = LexicalBackendConfig::default();
        let m = LexicalBackendMetrics {
            docs_ingested: 1000,
            queries_executed: 100,
            query_errors: 10, // 10% error rate
            ..Default::default()
        };
        let expl = explain_lexical_backend(&cfg, &m);
        assert!(expl.is_degraded);
        assert!(
            expl.degradation_reason
                .as_deref()
                .unwrap()
                .contains("query error")
        );
    }

    #[test]
    fn explain_high_rejections_degraded() {
        let cfg = LexicalBackendConfig::default();
        let m = LexicalBackendMetrics {
            docs_ingested: 700,
            docs_rejected: 300, // 30% rejection rate
            ..Default::default()
        };
        let expl = explain_lexical_backend(&cfg, &m);
        assert!(expl.is_degraded);
        assert!(
            expl.degradation_reason
                .as_deref()
                .unwrap()
                .contains("rejection")
        );
    }

    #[test]
    fn explain_high_churn_degraded() {
        let cfg = LexicalBackendConfig::default();
        let m = LexicalBackendMetrics {
            docs_ingested: 1000,
            docs_expired: 950, // 95% churn
            ..Default::default()
        };
        let expl = explain_lexical_backend(&cfg, &m);
        assert!(expl.is_degraded);
        assert!(
            expl.degradation_reason
                .as_deref()
                .unwrap()
                .contains("churn")
        );
    }

    #[test]
    fn explain_empty_metrics_not_degraded() {
        let cfg = LexicalBackendConfig::default();
        let m = LexicalBackendMetrics::default();
        let expl = explain_lexical_backend(&cfg, &m);
        assert!(!expl.is_degraded);
    }

    #[test]
    fn explain_serde_roundtrip() {
        let cfg = LexicalBackendConfig::default();
        let m = LexicalBackendMetrics::default();
        let expl = explain_lexical_backend(&cfg, &m);
        let json = serde_json::to_string(&expl).unwrap();
        let back: LexicalBackendExplanation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.is_degraded, expl.is_degraded);
        assert!((back.churn_rate - expl.churn_rate).abs() < 1e-10);
    }
}
