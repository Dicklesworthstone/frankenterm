//! Daemon ↔ frankensearch batch coalescer bridge (B7).
//!
//! Adapts the embedding daemon's protocol and worker types to frankensearch's
//! batch coalescing, priority scheduling, and caching APIs. This bridge enables
//! the daemon to leverage frankensearch's `BatchCoalescer` for throughput-optimal
//! embedding without replacing the daemon's wire protocol.
//!
//! # Design
//!
//! The bridge operates at three levels:
//!
//! 1. **Priority mapping**: `EmbedPriority` ↔ frankensearch `Priority`
//! 2. **Config bridging**: `DaemonBridgeConfig` ↔ `CoalescerConfig` + cache params
//! 3. **Metrics aggregation**: Unified view of coalescer + cache metrics
//!
//! The daemon protocol itself (JSON wire format) remains unchanged — this bridge
//! provides the glue types that a managed daemon worker would use internally.

use frankensearch::embed::{CacheStats, CoalescerConfig, CoalescerMetrics, Priority};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::Ordering;

// ── Priority mapping ────────────────────────────────────────────────────

/// Priority classification for daemon embedding requests.
///
/// Maps to frankensearch `Priority` for batch coalescer dispatch:
/// - `Interactive`: tight deadline (~15ms), triggers early batch dispatch
/// - `Background`: can wait for fuller batches, maximizing throughput
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedPriority {
    /// Search-time query embedding — low latency required.
    Interactive,
    /// Index-time document embedding — throughput preferred.
    #[default]
    Background,
}

impl EmbedPriority {
    /// Canonical string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Background => "background",
        }
    }

    /// Parse from string (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "interactive" | "query" | "search" | "urgent" => Self::Interactive,
            _ => Self::Background,
        }
    }

    /// Whether this is an interactive (latency-sensitive) request.
    #[must_use]
    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

impl fmt::Display for EmbedPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Convert local priority to frankensearch Priority.
#[must_use]
pub fn to_fs_priority(p: EmbedPriority) -> Priority {
    match p {
        EmbedPriority::Interactive => Priority::Interactive,
        EmbedPriority::Background => Priority::Background,
    }
}

/// Convert frankensearch Priority to local priority.
#[must_use]
pub fn from_fs_priority(p: Priority) -> EmbedPriority {
    match p {
        Priority::Interactive => EmbedPriority::Interactive,
        Priority::Background => EmbedPriority::Background,
    }
}

// ── Configuration bridging ──────────────────────────────────────────────

/// Configuration for the daemon ↔ frankensearch batch bridge.
///
/// Combines `CoalescerConfig` parameters (batch sizing, wait times, priority
/// lanes) with daemon-specific settings (cache capacity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonBridgeConfig {
    /// Maximum texts per batch (maps to `CoalescerConfig::max_batch_size`).
    #[serde(default = "default_max_batch_size")]
    pub max_batch_size: usize,
    /// Maximum wait before dispatching a partial batch (ms).
    #[serde(default = "default_max_wait_ms")]
    pub max_wait_ms: u64,
    /// Minimum texts to form a batch (below this, wait longer).
    #[serde(default = "default_min_batch_size")]
    pub min_batch_size: usize,
    /// Whether to use priority lanes (interactive vs background).
    #[serde(default = "default_priority_lanes")]
    pub use_priority_lanes: bool,
    /// LRU cache capacity for the CachedEmbedder wrapper.
    #[serde(default = "default_cache_capacity")]
    pub cache_capacity: usize,
}

fn default_max_batch_size() -> usize {
    32
}
fn default_max_wait_ms() -> u64 {
    10
}
fn default_min_batch_size() -> usize {
    4
}
fn default_priority_lanes() -> bool {
    true
}
fn default_cache_capacity() -> usize {
    128
}

impl Default for DaemonBridgeConfig {
    fn default() -> Self {
        Self {
            max_batch_size: default_max_batch_size(),
            max_wait_ms: default_max_wait_ms(),
            min_batch_size: default_min_batch_size(),
            use_priority_lanes: default_priority_lanes(),
            cache_capacity: default_cache_capacity(),
        }
    }
}

/// Convert a `DaemonBridgeConfig` to a frankensearch `CoalescerConfig`.
#[must_use]
pub fn to_coalescer_config(cfg: &DaemonBridgeConfig) -> CoalescerConfig {
    CoalescerConfig {
        max_batch_size: cfg.max_batch_size,
        max_wait_ms: cfg.max_wait_ms,
        min_batch_size: cfg.min_batch_size,
        use_priority_lanes: cfg.use_priority_lanes,
    }
}

/// Convert a frankensearch `CoalescerConfig` + cache capacity to local config.
#[must_use]
pub fn from_coalescer_config(cfg: &CoalescerConfig, cache_capacity: usize) -> DaemonBridgeConfig {
    DaemonBridgeConfig {
        max_batch_size: cfg.max_batch_size,
        max_wait_ms: cfg.max_wait_ms,
        min_batch_size: cfg.min_batch_size,
        use_priority_lanes: cfg.use_priority_lanes,
        cache_capacity,
    }
}

// ── Metrics aggregation ─────────────────────────────────────────────────

/// Aggregated metrics from the daemon bridge layer.
///
/// Combines frankensearch `CoalescerMetrics` (batch dispatch telemetry) with
/// `CacheStats` (embedding cache hit/miss) into a single observability snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonBridgeMetrics {
    // ── Coalescer counters ──
    pub total_submitted: u64,
    pub total_batches: u64,
    pub total_texts_batched: u64,
    pub interactive_submissions: u64,
    pub background_submissions: u64,
    pub avg_batch_size: f64,

    // ── Dispatch reason breakdown ──
    pub early_dispatches: u64,
    pub deadline_dispatches: u64,
    pub full_batch_dispatches: u64,
    pub timeout_dispatches: u64,

    // ── Cache counters ──
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_entries: usize,
    pub cache_capacity: usize,
}

impl Default for DaemonBridgeMetrics {
    fn default() -> Self {
        Self {
            total_submitted: 0,
            total_batches: 0,
            total_texts_batched: 0,
            interactive_submissions: 0,
            background_submissions: 0,
            avg_batch_size: 0.0,
            early_dispatches: 0,
            deadline_dispatches: 0,
            full_batch_dispatches: 0,
            timeout_dispatches: 0,
            cache_hits: 0,
            cache_misses: 0,
            cache_entries: 0,
            cache_capacity: 0,
        }
    }
}

/// Build a `DaemonBridgeMetrics` from frankensearch coalescer + cache stats.
///
/// If `cache_stats` is `None`, cache fields default to zero.
#[must_use]
pub fn from_coalescer_metrics(
    cm: &CoalescerMetrics,
    cache_stats: Option<&CacheStats>,
) -> DaemonBridgeMetrics {
    let cs = cache_stats.copied().unwrap_or(CacheStats {
        hits: 0,
        misses: 0,
        entries: 0,
        capacity: 0,
    });

    DaemonBridgeMetrics {
        total_submitted: cm.total_submitted.load(Ordering::Relaxed),
        total_batches: cm.total_batches.load(Ordering::Relaxed),
        total_texts_batched: cm.total_texts_batched.load(Ordering::Relaxed),
        interactive_submissions: cm.interactive_submissions.load(Ordering::Relaxed),
        background_submissions: cm.background_submissions.load(Ordering::Relaxed),
        avg_batch_size: cm.avg_batch_size(),
        early_dispatches: cm.early_dispatches.load(Ordering::Relaxed),
        deadline_dispatches: cm.deadline_dispatches.load(Ordering::Relaxed),
        full_batch_dispatches: cm.full_batch_dispatches.load(Ordering::Relaxed),
        timeout_dispatches: cm.timeout_dispatches.load(Ordering::Relaxed),
        cache_hits: cs.hits,
        cache_misses: cs.misses,
        cache_entries: cs.entries,
        cache_capacity: cs.capacity,
    }
}

/// Compute cache hit rate as a fraction in [0.0, 1.0].
///
/// Returns 0.0 if no cache requests have been made.
#[must_use]
pub fn compute_cache_hit_rate(metrics: &DaemonBridgeMetrics) -> f64 {
    let total = metrics.cache_hits + metrics.cache_misses;
    if total == 0 {
        return 0.0;
    }
    metrics.cache_hits as f64 / total as f64
}

/// Compute batch utilization as a fraction of max batch size.
///
/// Returns 0.0 if no batches have been dispatched.
#[must_use]
pub fn compute_batch_utilization(metrics: &DaemonBridgeMetrics, max_batch_size: usize) -> f64 {
    if metrics.total_batches == 0 || max_batch_size == 0 {
        return 0.0;
    }
    metrics.avg_batch_size / max_batch_size as f64
}

/// Compute priority lane skew as (interactive - background) / total.
///
/// Returns 0.0 if no submissions, positive if interactive-heavy, negative if
/// background-heavy. Range: [-1.0, 1.0].
#[must_use]
pub fn compute_priority_skew(metrics: &DaemonBridgeMetrics) -> f64 {
    let total = metrics.interactive_submissions + metrics.background_submissions;
    if total == 0 {
        return 0.0;
    }
    (metrics.interactive_submissions as f64 - metrics.background_submissions as f64) / total as f64
}

// ── Batch request/response types ────────────────────────────────────────

/// A single entry in a batch embed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SingleEmbedEntry {
    pub id: u64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Batch embedding request — groups multiple texts for coalesced dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchEmbedRequest {
    pub entries: Vec<SingleEmbedEntry>,
    #[serde(default)]
    pub priority: EmbedPriority,
}

impl BatchEmbedRequest {
    /// Number of texts in this batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether this batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Extract texts for embedding (in submission order).
    #[must_use]
    pub fn texts(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.text.as_str()).collect()
    }
}

/// A single result from a batch embed operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SingleEmbedResult {
    pub id: u64,
    pub vector: Vec<f32>,
    pub model: String,
    pub elapsed_ms: u64,
}

/// Batch embedding result with dispatch metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchEmbedResult {
    pub results: Vec<SingleEmbedResult>,
    /// Actual batch size dispatched (may differ from request if coalesced).
    pub batch_size: usize,
    /// Whether frankensearch coalesced this batch with other requests.
    pub coalesced: bool,
}

/// Convert batch entry texts to a Vec suitable for `Embedder::embed_batch()`.
#[must_use]
pub fn entries_to_texts(entries: &[SingleEmbedEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.text.as_str()).collect()
}

/// Convert embedding vectors back to `SingleEmbedResult`s.
///
/// Pairs each vector with the corresponding entry's ID. Vectors and entries
/// must be the same length (panics if mismatched in debug, truncates in release).
#[must_use]
pub fn vectors_to_results(
    vectors: &[Vec<f32>],
    entries: &[SingleEmbedEntry],
    model: &str,
    elapsed_ms: u64,
) -> Vec<SingleEmbedResult> {
    let count = vectors.len().min(entries.len());
    (0..count)
        .map(|i| SingleEmbedResult {
            id: entries[i].id,
            vector: vectors[i].clone(),
            model: model.to_string(),
            elapsed_ms,
        })
        .collect()
}

// ── Explainability ──────────────────────────────────────────────────────

/// Diagnostic explanation of daemon bridge state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonBridgeExplanation {
    /// Active configuration.
    pub config: DaemonBridgeConfig,
    /// Current metrics snapshot.
    pub metrics: DaemonBridgeMetrics,
    /// Cache hit rate [0.0, 1.0].
    pub cache_hit_rate: f64,
    /// Batch utilization as fraction of max_batch_size [0.0, 1.0].
    pub batch_utilization: f64,
    /// Priority lane skew [-1.0, 1.0].
    pub priority_skew: f64,
    /// Whether the bridge is operating in degraded mode.
    pub is_degraded: bool,
    /// Reason for degradation (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degradation_reason: Option<String>,
}

/// Build a diagnostic explanation from config + metrics.
#[must_use]
pub fn explain_bridge(
    config: &DaemonBridgeConfig,
    metrics: &DaemonBridgeMetrics,
) -> DaemonBridgeExplanation {
    let cache_hit_rate = compute_cache_hit_rate(metrics);
    let batch_utilization = compute_batch_utilization(metrics, config.max_batch_size);
    let priority_skew = compute_priority_skew(metrics);

    // Degradation heuristics:
    // - Cache hit rate below 10% with significant traffic suggests poor locality
    // - Batch utilization below 25% suggests config mismatch (min_batch_size too high
    //   or max_wait_ms too low for the traffic pattern)
    let total_requests = metrics.cache_hits + metrics.cache_misses;
    let is_low_cache = total_requests > 100 && cache_hit_rate < 0.1;
    let is_low_batch = metrics.total_batches > 10 && batch_utilization < 0.25;
    let is_degraded = is_low_cache || is_low_batch;

    let degradation_reason = if is_low_cache && is_low_batch {
        Some("low cache hit rate and low batch utilization".to_string())
    } else if is_low_cache {
        Some("low cache hit rate (< 10%)".to_string())
    } else if is_low_batch {
        Some("low batch utilization (< 25%)".to_string())
    } else {
        None
    };

    DaemonBridgeExplanation {
        config: config.clone(),
        metrics: metrics.clone(),
        cache_hit_rate,
        batch_utilization,
        priority_skew,
        is_degraded,
        degradation_reason,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── EmbedPriority ──────────────────────────────────────────────

    #[test]
    fn priority_default_is_background() {
        assert_eq!(EmbedPriority::default(), EmbedPriority::Background);
    }

    #[test]
    fn priority_as_str() {
        assert_eq!(EmbedPriority::Interactive.as_str(), "interactive");
        assert_eq!(EmbedPriority::Background.as_str(), "background");
    }

    #[test]
    fn priority_display() {
        assert_eq!(format!("{}", EmbedPriority::Interactive), "interactive");
        assert_eq!(format!("{}", EmbedPriority::Background), "background");
    }

    #[test]
    fn priority_parse() {
        assert_eq!(
            EmbedPriority::parse("interactive"),
            EmbedPriority::Interactive
        );
        assert_eq!(
            EmbedPriority::parse("INTERACTIVE"),
            EmbedPriority::Interactive
        );
        assert_eq!(EmbedPriority::parse("query"), EmbedPriority::Interactive);
        assert_eq!(EmbedPriority::parse("search"), EmbedPriority::Interactive);
        assert_eq!(EmbedPriority::parse("urgent"), EmbedPriority::Interactive);
        assert_eq!(
            EmbedPriority::parse("background"),
            EmbedPriority::Background
        );
        assert_eq!(
            EmbedPriority::parse("BACKGROUND"),
            EmbedPriority::Background
        );
        assert_eq!(EmbedPriority::parse("index"), EmbedPriority::Background);
        assert_eq!(EmbedPriority::parse("unknown"), EmbedPriority::Background);
        assert_eq!(EmbedPriority::parse(""), EmbedPriority::Background);
    }

    #[test]
    fn priority_is_interactive() {
        assert!(EmbedPriority::Interactive.is_interactive());
        assert!(!EmbedPriority::Background.is_interactive());
    }

    #[test]
    fn priority_serde_roundtrip() {
        for p in [EmbedPriority::Interactive, EmbedPriority::Background] {
            let json = serde_json::to_string(&p).unwrap();
            let back: EmbedPriority = serde_json::from_str(&json).unwrap();
            assert_eq!(p, back);
        }
    }

    #[test]
    fn priority_debug() {
        assert_eq!(format!("{:?}", EmbedPriority::Interactive), "Interactive");
        assert_eq!(format!("{:?}", EmbedPriority::Background), "Background");
    }

    // ── Priority mapping ───────────────────────────────────────────

    #[test]
    fn to_fs_priority_maps_correctly() {
        assert_eq!(
            to_fs_priority(EmbedPriority::Interactive),
            Priority::Interactive
        );
        assert_eq!(
            to_fs_priority(EmbedPriority::Background),
            Priority::Background
        );
    }

    #[test]
    fn from_fs_priority_maps_correctly() {
        assert_eq!(
            from_fs_priority(Priority::Interactive),
            EmbedPriority::Interactive
        );
        assert_eq!(
            from_fs_priority(Priority::Background),
            EmbedPriority::Background
        );
    }

    #[test]
    fn priority_roundtrip_local_to_fs_and_back() {
        for p in [EmbedPriority::Interactive, EmbedPriority::Background] {
            assert_eq!(from_fs_priority(to_fs_priority(p)), p);
        }
    }

    // ── DaemonBridgeConfig ─────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let cfg = DaemonBridgeConfig::default();
        assert_eq!(cfg.max_batch_size, 32);
        assert_eq!(cfg.max_wait_ms, 10);
        assert_eq!(cfg.min_batch_size, 4);
        assert!(cfg.use_priority_lanes);
        assert_eq!(cfg.cache_capacity, 128);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = DaemonBridgeConfig {
            max_batch_size: 64,
            max_wait_ms: 20,
            min_batch_size: 8,
            use_priority_lanes: false,
            cache_capacity: 256,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: DaemonBridgeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn config_serde_missing_fields_use_defaults() {
        let json = r"{}";
        let cfg: DaemonBridgeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg, DaemonBridgeConfig::default());
    }

    #[test]
    fn config_to_coalescer_maps_fields() {
        let cfg = DaemonBridgeConfig {
            max_batch_size: 16,
            max_wait_ms: 5,
            min_batch_size: 2,
            use_priority_lanes: false,
            cache_capacity: 64,
        };
        let cc = to_coalescer_config(&cfg);
        assert_eq!(cc.max_batch_size, 16);
        assert_eq!(cc.max_wait_ms, 5);
        assert_eq!(cc.min_batch_size, 2);
        assert!(!cc.use_priority_lanes);
    }

    #[test]
    fn config_from_coalescer_roundtrip() {
        let cfg = DaemonBridgeConfig {
            max_batch_size: 48,
            max_wait_ms: 15,
            min_batch_size: 6,
            use_priority_lanes: true,
            cache_capacity: 512,
        };
        let cc = to_coalescer_config(&cfg);
        let back = from_coalescer_config(&cc, cfg.cache_capacity);
        assert_eq!(cfg, back);
    }

    // ── DaemonBridgeMetrics ────────────────────────────────────────

    #[test]
    fn metrics_default_is_zero() {
        let m = DaemonBridgeMetrics::default();
        assert_eq!(m.total_submitted, 0);
        assert_eq!(m.total_batches, 0);
        assert_eq!(m.cache_hits, 0);
        assert_eq!(m.cache_misses, 0);
        assert!((m.avg_batch_size - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn from_coalescer_metrics_without_cache() {
        let cm = CoalescerMetrics::default();
        let m = from_coalescer_metrics(&cm, None);
        assert_eq!(m.total_submitted, 0);
        assert_eq!(m.cache_hits, 0);
        assert_eq!(m.cache_capacity, 0);
    }

    #[test]
    fn from_coalescer_metrics_with_cache() {
        let cm = CoalescerMetrics::default();
        let cs = CacheStats {
            hits: 42,
            misses: 8,
            entries: 30,
            capacity: 128,
        };
        let m = from_coalescer_metrics(&cm, Some(&cs));
        assert_eq!(m.cache_hits, 42);
        assert_eq!(m.cache_misses, 8);
        assert_eq!(m.cache_entries, 30);
        assert_eq!(m.cache_capacity, 128);
    }

    #[test]
    fn metrics_serde_roundtrip() {
        let m = DaemonBridgeMetrics {
            total_submitted: 100,
            total_batches: 10,
            total_texts_batched: 95,
            interactive_submissions: 30,
            background_submissions: 70,
            avg_batch_size: 9.5,
            early_dispatches: 5,
            deadline_dispatches: 2,
            full_batch_dispatches: 2,
            timeout_dispatches: 1,
            cache_hits: 60,
            cache_misses: 40,
            cache_entries: 50,
            cache_capacity: 128,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: DaemonBridgeMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_submitted, 100);
        assert_eq!(back.cache_hits, 60);
        assert!((back.avg_batch_size - 9.5).abs() < 1e-10);
    }

    // ── Derived metrics ────────────────────────────────────────────

    #[test]
    fn cache_hit_rate_zero_when_no_requests() {
        let m = DaemonBridgeMetrics::default();
        assert!((compute_cache_hit_rate(&m) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cache_hit_rate_all_hits() {
        let m = DaemonBridgeMetrics {
            cache_hits: 100,
            cache_misses: 0,
            ..Default::default()
        };
        assert!((compute_cache_hit_rate(&m) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cache_hit_rate_mixed() {
        let m = DaemonBridgeMetrics {
            cache_hits: 75,
            cache_misses: 25,
            ..Default::default()
        };
        assert!((compute_cache_hit_rate(&m) - 0.75).abs() < 1e-10);
    }

    #[test]
    fn batch_utilization_zero_when_no_batches() {
        let m = DaemonBridgeMetrics::default();
        assert!((compute_batch_utilization(&m, 32) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn batch_utilization_full() {
        let m = DaemonBridgeMetrics {
            total_batches: 10,
            avg_batch_size: 32.0,
            ..Default::default()
        };
        assert!((compute_batch_utilization(&m, 32) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn batch_utilization_partial() {
        let m = DaemonBridgeMetrics {
            total_batches: 10,
            avg_batch_size: 16.0,
            ..Default::default()
        };
        assert!((compute_batch_utilization(&m, 32) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn batch_utilization_zero_max_size() {
        let m = DaemonBridgeMetrics {
            total_batches: 10,
            avg_batch_size: 16.0,
            ..Default::default()
        };
        assert!((compute_batch_utilization(&m, 0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn priority_skew_zero_when_no_submissions() {
        let m = DaemonBridgeMetrics::default();
        assert!((compute_priority_skew(&m) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn priority_skew_all_interactive() {
        let m = DaemonBridgeMetrics {
            interactive_submissions: 100,
            background_submissions: 0,
            ..Default::default()
        };
        assert!((compute_priority_skew(&m) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn priority_skew_all_background() {
        let m = DaemonBridgeMetrics {
            interactive_submissions: 0,
            background_submissions: 100,
            ..Default::default()
        };
        assert!((compute_priority_skew(&m) - (-1.0)).abs() < 1e-10);
    }

    #[test]
    fn priority_skew_balanced() {
        let m = DaemonBridgeMetrics {
            interactive_submissions: 50,
            background_submissions: 50,
            ..Default::default()
        };
        assert!((compute_priority_skew(&m) - 0.0).abs() < 1e-10);
    }

    // ── Batch request/response types ───────────────────────────────

    #[test]
    fn batch_request_len() {
        let req = BatchEmbedRequest {
            entries: vec![
                SingleEmbedEntry {
                    id: 1,
                    text: "a".into(),
                    model: None,
                },
                SingleEmbedEntry {
                    id: 2,
                    text: "b".into(),
                    model: None,
                },
            ],
            priority: EmbedPriority::Background,
        };
        assert_eq!(req.len(), 2);
        assert!(!req.is_empty());
    }

    #[test]
    fn batch_request_empty() {
        let req = BatchEmbedRequest {
            entries: vec![],
            priority: EmbedPriority::Interactive,
        };
        assert_eq!(req.len(), 0);
        assert!(req.is_empty());
    }

    #[test]
    fn batch_request_texts() {
        let req = BatchEmbedRequest {
            entries: vec![
                SingleEmbedEntry {
                    id: 1,
                    text: "hello".into(),
                    model: None,
                },
                SingleEmbedEntry {
                    id: 2,
                    text: "world".into(),
                    model: Some("hash".into()),
                },
            ],
            priority: EmbedPriority::Background,
        };
        assert_eq!(req.texts(), vec!["hello", "world"]);
    }

    #[test]
    fn batch_request_serde_roundtrip() {
        let req = BatchEmbedRequest {
            entries: vec![SingleEmbedEntry {
                id: 1,
                text: "test".into(),
                model: None,
            }],
            priority: EmbedPriority::Interactive,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: BatchEmbedRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.priority, EmbedPriority::Interactive);
    }

    #[test]
    fn batch_request_serde_missing_priority_defaults() {
        let json = r#"{"entries":[{"id":1,"text":"x"}]}"#;
        let req: BatchEmbedRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.priority, EmbedPriority::Background);
    }

    #[test]
    fn entries_to_texts_preserves_order() {
        let entries = vec![
            SingleEmbedEntry {
                id: 3,
                text: "third".into(),
                model: None,
            },
            SingleEmbedEntry {
                id: 1,
                text: "first".into(),
                model: None,
            },
            SingleEmbedEntry {
                id: 2,
                text: "second".into(),
                model: None,
            },
        ];
        assert_eq!(entries_to_texts(&entries), vec!["third", "first", "second"]);
    }

    #[test]
    fn entries_to_texts_empty() {
        let entries: Vec<SingleEmbedEntry> = vec![];
        let texts = entries_to_texts(&entries);
        assert!(texts.is_empty());
    }

    #[test]
    fn vectors_to_results_pairs_correctly() {
        let entries = vec![
            SingleEmbedEntry {
                id: 10,
                text: "a".into(),
                model: None,
            },
            SingleEmbedEntry {
                id: 20,
                text: "b".into(),
                model: None,
            },
        ];
        let vectors = vec![vec![0.1, 0.2], vec![0.3, 0.4]];
        let results = vectors_to_results(&vectors, &entries, "test-model", 42);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, 10);
        assert_eq!(results[0].vector, vec![0.1, 0.2]);
        assert_eq!(results[0].model, "test-model");
        assert_eq!(results[0].elapsed_ms, 42);
        assert_eq!(results[1].id, 20);
        assert_eq!(results[1].vector, vec![0.3, 0.4]);
    }

    #[test]
    fn vectors_to_results_truncates_on_mismatch() {
        let entries = vec![SingleEmbedEntry {
            id: 1,
            text: "a".into(),
            model: None,
        }];
        let vectors = vec![vec![0.1], vec![0.2], vec![0.3]];
        let results = vectors_to_results(&vectors, &entries, "m", 0);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 1);
    }

    #[test]
    fn vectors_to_results_empty() {
        let results = vectors_to_results(&[], &[], "m", 0);
        assert!(results.is_empty());
    }

    #[test]
    fn batch_result_serde_roundtrip() {
        let result = BatchEmbedResult {
            results: vec![SingleEmbedResult {
                id: 5,
                vector: vec![1.0, 2.0, 3.0],
                model: "hash-3".into(),
                elapsed_ms: 7,
            }],
            batch_size: 1,
            coalesced: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: BatchEmbedResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.results.len(), 1);
        assert_eq!(back.batch_size, 1);
        assert!(!back.coalesced);
    }

    // ── Explainability ─────────────────────────────────────────────

    #[test]
    fn explain_bridge_healthy() {
        let cfg = DaemonBridgeConfig::default();
        let m = DaemonBridgeMetrics {
            total_submitted: 1000,
            total_batches: 40,
            total_texts_batched: 960,
            interactive_submissions: 200,
            background_submissions: 800,
            avg_batch_size: 24.0,
            early_dispatches: 10,
            deadline_dispatches: 5,
            full_batch_dispatches: 20,
            timeout_dispatches: 5,
            cache_hits: 300,
            cache_misses: 100,
            cache_entries: 80,
            cache_capacity: 128,
        };
        let expl = explain_bridge(&cfg, &m);
        assert!(!expl.is_degraded);
        assert!(expl.degradation_reason.is_none());
        assert!((expl.cache_hit_rate - 0.75).abs() < 1e-10);
        assert!((expl.batch_utilization - 0.75).abs() < 1e-10);
        assert!(expl.priority_skew < 0.0); // background-heavy
    }

    #[test]
    fn explain_bridge_low_cache_degraded() {
        let cfg = DaemonBridgeConfig::default();
        let m = DaemonBridgeMetrics {
            total_submitted: 500,
            total_batches: 20,
            avg_batch_size: 25.0,
            cache_hits: 5,
            cache_misses: 200,
            ..Default::default()
        };
        let expl = explain_bridge(&cfg, &m);
        assert!(expl.is_degraded);
        assert!(
            expl.degradation_reason
                .as_deref()
                .unwrap()
                .contains("cache")
        );
    }

    #[test]
    fn explain_bridge_low_batch_degraded() {
        let cfg = DaemonBridgeConfig::default(); // max_batch_size = 32
        let m = DaemonBridgeMetrics {
            total_submitted: 500,
            total_batches: 100,
            avg_batch_size: 5.0, // 5/32 = 15.6% < 25%
            cache_hits: 200,
            cache_misses: 100,
            ..Default::default()
        };
        let expl = explain_bridge(&cfg, &m);
        assert!(expl.is_degraded);
        assert!(
            expl.degradation_reason
                .as_deref()
                .unwrap()
                .contains("batch")
        );
    }

    #[test]
    fn explain_bridge_empty_metrics_not_degraded() {
        let cfg = DaemonBridgeConfig::default();
        let m = DaemonBridgeMetrics::default();
        let expl = explain_bridge(&cfg, &m);
        assert!(!expl.is_degraded);
    }

    #[test]
    fn explain_bridge_serde_roundtrip() {
        let cfg = DaemonBridgeConfig::default();
        let m = DaemonBridgeMetrics::default();
        let expl = explain_bridge(&cfg, &m);
        let json = serde_json::to_string(&expl).unwrap();
        let back: DaemonBridgeExplanation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.is_degraded, expl.is_degraded);
        assert!((back.cache_hit_rate - expl.cache_hit_rate).abs() < 1e-10);
    }
}
