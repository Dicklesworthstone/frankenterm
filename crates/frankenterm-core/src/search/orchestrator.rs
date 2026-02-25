//! Search orchestration layer for frankensearch migration.
//!
//! Provides a `SearchOrchestrator` that encapsulates the full search pipeline:
//! query → lexical + semantic retrieval → fusion → ranked results.
//!
//! Two backends are available:
//!
//! - **Legacy** (`OrchestrationBackend::Legacy`): Uses `HybridSearchService::fuse()`
//!   directly with caller-provided lexical/semantic ranked lists. This is the current
//!   production path that storage.rs uses.
//!
//! - **Bridge** (`OrchestrationBackend::Bridge`): Delegates full orchestration to
//!   `SearchBridge` / `TwoTierSearcher`, which internally manages lexical retrieval,
//!   semantic retrieval, and RRF fusion. Requires the `frankensearch` feature.
//!
//! The backend is selected at construction time and can be overridden via the
//! `FT_SEARCH_ORCHESTRATION` environment variable (`legacy` or `bridge`).
//!
//! # Embedder dispatch (B3)
//!
//! The orchestrator supports two embedder dispatch strategies:
//!
//! - **Legacy**: Caller embeds externally and provides pre-ranked lists.
//! - **Managed**: Orchestrator owns a `ManagedEmbedderStack` with tiered fallback
//!   (Quality → Fast → Hash). The stack auto-detects available models and
//!   degrades gracefully to hash embeddings when ONNX/distilled models are
//!   unavailable.
//!
//! # Migration path
//!
//! 1. A1 (done): Freeze API contract + baseline regression corpus
//! 2. B1 (done): Create orchestration abstraction with legacy + bridge backends
//! 3. B2 (done): Weight-aware frankensearch RRF fusion + bridge fallback
//! 4. **B3 (done)**: Migrate embedder stack dispatch + fallback tiers
//! 5. B4: Migrate vector/chunk index internals

use super::{
    EmbedError, Embedder, EmbedderInfo, EmbedderTier, FusedResult, HashEmbedder,
    HybridSearchService, SearchMode, TwoTierMetrics,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

/// Serializable search mode selector (mirrors SearchMode but with serde).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchModeConfig {
    Lexical,
    Semantic,
    Hybrid,
}

impl From<SearchModeConfig> for SearchMode {
    fn from(val: SearchModeConfig) -> Self {
        match val {
            SearchModeConfig::Lexical => SearchMode::Lexical,
            SearchModeConfig::Semantic => SearchMode::Semantic,
            SearchModeConfig::Hybrid => SearchMode::Hybrid,
        }
    }
}

impl From<SearchMode> for SearchModeConfig {
    fn from(val: SearchMode) -> Self {
        match val {
            SearchMode::Lexical => SearchModeConfig::Lexical,
            SearchMode::Semantic => SearchModeConfig::Semantic,
            SearchMode::Hybrid => SearchModeConfig::Hybrid,
        }
    }
}

// ── B3: Embedder dispatch types ───────────────────────────────────────

/// Embedder dispatch strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedderDispatch {
    /// Caller embeds externally and provides pre-ranked lists.
    Legacy,
    /// Orchestrator manages an embedder stack with tiered fallback.
    Managed,
}

impl Default for EmbedderDispatch {
    fn default() -> Self {
        Self::Legacy
    }
}

/// Availability tier of the managed embedder stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedderAvailability {
    /// Quality + fast + hash all available.
    Full,
    /// Fast + hash available, quality model missing.
    FastOnly,
    /// Hash-only fallback (no semantic models).
    HashOnly,
    /// No embedder configured (legacy dispatch).
    None,
}

impl EmbedderAvailability {
    /// Whether this represents a degraded state (missing tiers).
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::FastOnly | Self::HashOnly)
    }
}

impl fmt::Display for EmbedderAvailability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => write!(f, "full (quality + fast + hash)"),
            Self::FastOnly => write!(f, "degraded (fast + hash, no quality)"),
            Self::HashOnly => write!(f, "minimal (hash only)"),
            Self::None => write!(f, "none (legacy dispatch)"),
        }
    }
}

/// Managed embedder stack with tiered fallback.
///
/// Wraps one or more `Embedder` implementations with automatic fallback:
/// Quality → Fast → Hash. Tracks which tier was actually used for metrics.
pub struct ManagedEmbedderStack {
    /// Quality-tier embedder (e.g. FastEmbed ONNX). Optional.
    quality: Option<Arc<dyn Embedder>>,
    /// Fast-tier embedder (e.g. Model2Vec distilled). Optional.
    fast: Option<Arc<dyn Embedder>>,
    /// Hash-tier embedder (always available).
    hash: Arc<dyn Embedder>,
    /// Current availability classification.
    availability: EmbedderAvailability,
}

impl ManagedEmbedderStack {
    /// Create a hash-only stack (always succeeds).
    #[must_use]
    pub fn hash_only() -> Self {
        Self {
            quality: None,
            fast: None,
            hash: Arc::new(HashEmbedder::default()),
            availability: EmbedderAvailability::HashOnly,
        }
    }

    /// Create from explicit tier parts.
    #[must_use]
    pub fn from_tiers(
        quality: Option<Arc<dyn Embedder>>,
        fast: Option<Arc<dyn Embedder>>,
        hash: Arc<dyn Embedder>,
    ) -> Self {
        let availability = match (&quality, &fast) {
            (Some(_), _) => EmbedderAvailability::Full,
            (None, Some(_)) => EmbedderAvailability::FastOnly,
            (None, None) => EmbedderAvailability::HashOnly,
        };
        Self {
            quality,
            fast,
            hash,
            availability,
        }
    }

    /// Current availability classification.
    #[must_use]
    pub fn availability(&self) -> EmbedderAvailability {
        self.availability
    }

    /// Get the best available embedder, falling back through tiers.
    ///
    /// Returns the embedder and which tier was selected.
    #[must_use]
    pub fn best_embedder(&self) -> (&dyn Embedder, EmbedderTier) {
        if let Some(ref q) = self.quality {
            return (q.as_ref(), EmbedderTier::Quality);
        }
        if let Some(ref f) = self.fast {
            return (f.as_ref(), EmbedderTier::Fast);
        }
        (self.hash.as_ref(), EmbedderTier::Hash)
    }

    /// Get a specific tier's embedder, if available.
    #[must_use]
    pub fn embedder_for_tier(&self, tier: EmbedderTier) -> Option<&dyn Embedder> {
        match tier {
            EmbedderTier::Quality => self.quality.as_deref(),
            EmbedderTier::Fast => self.fast.as_deref(),
            EmbedderTier::Hash => Some(self.hash.as_ref()),
        }
    }

    /// Embed text using the best available embedder, with fallback on error.
    ///
    /// Tries Quality → Fast → Hash. Returns the embedding vector and which
    /// tier actually produced it.
    pub fn embed_with_fallback(&self, text: &str) -> Result<(Vec<f32>, EmbedderTier), EmbedError> {
        // Try quality tier first
        if let Some(ref q) = self.quality {
            match q.embed(text) {
                Ok(v) => return Ok((v, EmbedderTier::Quality)),
                Err(_) => { /* fall through to fast */ }
            }
        }
        // Try fast tier
        if let Some(ref f) = self.fast {
            match f.embed(text) {
                Ok(v) => return Ok((v, EmbedderTier::Fast)),
                Err(_) => { /* fall through to hash */ }
            }
        }
        // Hash always succeeds
        let v = self.hash.embed(text)?;
        Ok((v, EmbedderTier::Hash))
    }

    /// Info for the best available embedder.
    #[must_use]
    pub fn best_info(&self) -> EmbedderInfo {
        let (emb, _) = self.best_embedder();
        emb.info()
    }
}

impl fmt::Debug for ManagedEmbedderStack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedEmbedderStack")
            .field("availability", &self.availability)
            .field("quality", &self.quality.as_ref().map(|e| e.info().name))
            .field("fast", &self.fast.as_ref().map(|e| e.info().name))
            .field("hash", &self.hash.info().name)
            .finish()
    }
}

// ── Backend + metrics types ───────────────────────────────────────────

/// Backend selector for search orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationBackend {
    /// Legacy: caller provides ranked lists, fuse via HybridSearchService.
    Legacy,
    /// Bridge: delegate full pipeline to TwoTierSearcher via SearchBridge.
    Bridge,
}

impl OrchestrationBackend {
    /// Parse from string (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "bridge" | "frankensearch" | "two_tier" | "twotier" => Self::Bridge,
            _ => Self::Legacy,
        }
    }

    /// Resolve from `FT_SEARCH_ORCHESTRATION` environment variable.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("FT_SEARCH_ORCHESTRATION") {
            Ok(val) => Self::parse(&val),
            Err(_) => Self::Legacy,
        }
    }

    /// Canonical string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Bridge => "bridge",
        }
    }
}

impl fmt::Display for OrchestrationBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Orchestration-level metrics, superset of fusion metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrchestrationMetrics {
    /// Backend that was used.
    pub backend: String,
    /// Search mode that was effective.
    pub effective_mode: String,
    /// Whether the backend fell back from bridge to legacy.
    pub fallback_occurred: bool,
    /// Reason for fallback (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    /// Number of lexical candidates provided/generated.
    pub lexical_candidates: usize,
    /// Number of semantic candidates provided/generated.
    pub semantic_candidates: usize,
    /// Two-tier fusion metrics (from HybridSearchService).
    pub fusion: TwoTierMetrics,
    /// Embedder dispatch strategy used.
    pub embedder_dispatch: String,
    /// Embedder availability at query time.
    pub embedder_availability: String,
    /// Which embedder tier actually produced the query embedding (if managed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedder_tier_used: Option<String>,
    /// Whether an embedder fallback occurred (e.g. quality→hash).
    pub embedder_fallback: bool,
}

/// Input for legacy orchestration: pre-ranked lists from caller.
#[derive(Debug, Clone)]
pub struct LegacySearchInput {
    /// Lexical ranked list: (doc_id, score) in descending score order.
    pub lexical_ranked: Vec<(u64, f32)>,
    /// Semantic ranked list: (doc_id, score) in descending score order.
    pub semantic_ranked: Vec<(u64, f32)>,
    /// Maximum results to return.
    pub top_k: usize,
}

/// Result from orchestrated search.
#[derive(Debug, Clone)]
pub struct OrchestrationResult {
    /// Fused ranked results.
    pub results: Vec<FusedResult>,
    /// Orchestration-level metrics.
    pub metrics: OrchestrationMetrics,
}

/// Configuration for the search orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// Which backend to use.
    pub backend: OrchestrationBackend,
    /// Search mode.
    pub mode: SearchModeConfig,
    /// RRF parameter k.
    pub rrf_k: u32,
    /// Two-tier blending alpha (0.0 = all tier2, 1.0 = all tier1).
    pub alpha: f32,
    /// Lexical lane weight for weighted RRF.
    pub lexical_weight: f32,
    /// Semantic lane weight for weighted RRF.
    pub semantic_weight: f32,
    /// Whether to fall back to legacy if bridge fails.
    pub fallback_to_legacy: bool,
    /// Embedder dispatch strategy.
    pub embedder_dispatch: EmbedderDispatch,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            backend: OrchestrationBackend::from_env(),
            mode: SearchModeConfig::Hybrid,
            rrf_k: 60,
            alpha: 0.7,
            lexical_weight: 1.0,
            semantic_weight: 1.0,
            fallback_to_legacy: true,
            embedder_dispatch: EmbedderDispatch::Legacy,
        }
    }
}

/// Search orchestrator that encapsulates backend selection and fallback logic.
///
/// In the legacy path, the caller provides pre-ranked lexical and semantic lists.
/// In the bridge path, the orchestrator delegates fusion to frankensearch with
/// weight-aware RRF scoring.
///
/// With managed embedder dispatch (B3), the orchestrator owns a `ManagedEmbedderStack`
/// and handles embedding with automatic fallback across tiers.
///
/// Both paths produce the same `OrchestrationResult` with compatible `FusedResult`
/// vectors, preserving the frozen API contract.
pub struct SearchOrchestrator {
    config: OrchestratorConfig,
    embedder_stack: Option<ManagedEmbedderStack>,
}

impl SearchOrchestrator {
    /// Create a new orchestrator with the given configuration.
    #[must_use]
    pub fn new(config: OrchestratorConfig) -> Self {
        Self {
            config,
            embedder_stack: None,
        }
    }

    /// Attach a managed embedder stack, switching dispatch to Managed.
    #[must_use]
    pub fn with_embedder_stack(mut self, stack: ManagedEmbedderStack) -> Self {
        self.config.embedder_dispatch = EmbedderDispatch::Managed;
        self.embedder_stack = Some(stack);
        self
    }

    /// Create with default config (resolves backend from env).
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(OrchestratorConfig::default())
    }

    /// Get the current configuration.
    #[must_use]
    pub fn config(&self) -> &OrchestratorConfig {
        &self.config
    }

    /// Get the active backend.
    #[must_use]
    pub fn backend(&self) -> OrchestrationBackend {
        self.config.backend
    }

    /// Get the embedder availability (None if legacy dispatch).
    #[must_use]
    pub fn embedder_availability(&self) -> EmbedderAvailability {
        self.embedder_stack
            .as_ref()
            .map_or(EmbedderAvailability::None, |s| s.availability())
    }

    /// Embed a query using the managed embedder stack (with fallback).
    ///
    /// Returns `None` if no managed stack is configured (legacy dispatch).
    pub fn embed_query(&self, text: &str) -> Option<Result<(Vec<f32>, EmbedderTier), EmbedError>> {
        self.embedder_stack
            .as_ref()
            .map(|stack| stack.embed_with_fallback(text))
    }

    /// Build embedder-related metrics based on current stack state.
    fn embedder_metrics(&self) -> (String, String, Option<String>, bool) {
        let dispatch = format!("{:?}", self.config.embedder_dispatch);
        let availability = format!("{}", self.embedder_availability());
        (dispatch, availability, None, false)
    }

    /// Execute search using pre-ranked lists (legacy-compatible entry point).
    ///
    /// This is the synchronous orchestration path used by `storage.rs`. Both the
    /// legacy and bridge backends can accept this input — the bridge path would
    /// ignore the pre-ranked lists and run its own retrieval, but we haven't
    /// implemented that yet (deferred to B2–B4).
    ///
    /// For now, both backends use the legacy fusion path, with the bridge backend
    /// adding instrumentation and metrics hooks for migration observability.
    pub fn fuse_ranked(&self, input: &LegacySearchInput) -> OrchestrationResult {
        match self.config.backend {
            OrchestrationBackend::Legacy => self.fuse_legacy(input),
            OrchestrationBackend::Bridge => self.fuse_bridge(input),
        }
    }

    /// Legacy fusion: delegate directly to HybridSearchService.
    fn fuse_legacy(&self, input: &LegacySearchInput) -> OrchestrationResult {
        let svc = HybridSearchService::new()
            .with_mode(SearchMode::from(self.config.mode))
            .with_rrf_k(self.config.rrf_k)
            .with_alpha(self.config.alpha)
            .with_rrf_weights(self.config.lexical_weight, self.config.semantic_weight);

        let results = svc.fuse(
            &input.lexical_ranked,
            &input.semantic_ranked,
            input.top_k,
        );

        let (emb_dispatch, emb_avail, emb_tier, emb_fallback) = self.embedder_metrics();

        OrchestrationResult {
            results,
            metrics: OrchestrationMetrics {
                backend: "legacy".to_string(),
                effective_mode: format!("{:?}", SearchMode::from(self.config.mode)),
                fallback_occurred: false,
                fallback_reason: None,
                lexical_candidates: input.lexical_ranked.len(),
                semantic_candidates: input.semantic_ranked.len(),
                fusion: TwoTierMetrics::default(),
                embedder_dispatch: emb_dispatch,
                embedder_availability: emb_avail,
                embedder_tier_used: emb_tier,
                embedder_fallback: emb_fallback,
            },
        }
    }

    /// Bridge fusion: delegates to frankensearch with weight-aware RRF scoring.
    ///
    /// Uses `HybridSearchService` configured with FrankenSearchRrf backend, which
    /// calls `frankensearch::rrf_fuse()` for rank assignment and then recomputes
    /// weighted scores using the local `rrf_component_score()` formula.
    ///
    /// If the bridge path panics (e.g. frankensearch internal error), and
    /// `fallback_to_legacy` is enabled, falls back to the legacy path and
    /// records the fallback in metrics.
    fn fuse_bridge(&self, input: &LegacySearchInput) -> OrchestrationResult {
        let bridge_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let svc = HybridSearchService::new()
                .with_mode(SearchMode::from(self.config.mode))
                .with_rrf_k(self.config.rrf_k)
                .with_alpha(self.config.alpha)
                .with_fusion_backend(super::FusionBackend::FrankenSearchRrf)
                .with_rrf_weights(self.config.lexical_weight, self.config.semantic_weight);

            svc.fuse(
                &input.lexical_ranked,
                &input.semantic_ranked,
                input.top_k,
            )
        }));

        let (emb_dispatch, emb_avail, emb_tier, emb_fallback) = self.embedder_metrics();

        match bridge_result {
            Ok(results) => OrchestrationResult {
                results,
                metrics: OrchestrationMetrics {
                    backend: "bridge".to_string(),
                    effective_mode: format!("{:?}", SearchMode::from(self.config.mode)),
                    fallback_occurred: false,
                    fallback_reason: None,
                    lexical_candidates: input.lexical_ranked.len(),
                    semantic_candidates: input.semantic_ranked.len(),
                    fusion: TwoTierMetrics::default(),
                    embedder_dispatch: emb_dispatch,
                    embedder_availability: emb_avail,
                    embedder_tier_used: emb_tier,
                    embedder_fallback: emb_fallback,
                },
            },
            Err(_) if self.config.fallback_to_legacy => {
                let mut result = self.fuse_legacy(input);
                result.metrics.fallback_occurred = true;
                result.metrics.fallback_reason =
                    Some("bridge_panic: fell back to legacy fusion".to_string());
                result.metrics.backend = "bridge(fallback->legacy)".to_string();
                result
            }
            Err(panic_payload) => std::panic::resume_unwind(panic_payload),
        }
    }

    /// Compare legacy and bridge results for the same input (migration validation).
    ///
    /// Returns both results and a comparison report. Use this during the migration
    /// to validate that the bridge path produces equivalent results to legacy.
    pub fn compare_backends(&self, input: &LegacySearchInput) -> OrchestrationComparison {
        let legacy = self.fuse_legacy(input);
        let bridge = self.fuse_bridge(input);

        let legacy_ids: Vec<u64> = legacy.results.iter().map(|r| r.id).collect();
        let bridge_ids: Vec<u64> = bridge.results.iter().map(|r| r.id).collect();
        let ranking_match = legacy_ids == bridge_ids;

        let score_diff = if legacy.results.len() == bridge.results.len() {
            legacy
                .results
                .iter()
                .zip(bridge.results.iter())
                .map(|(a, b)| (a.score - b.score).abs())
                .fold(0.0_f32, f32::max)
        } else {
            f32::INFINITY
        };

        OrchestrationComparison {
            legacy,
            bridge,
            ranking_match,
            max_score_diff: score_diff,
        }
    }
}

impl fmt::Debug for SearchOrchestrator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchOrchestrator")
            .field("config", &self.config)
            .finish()
    }
}

/// Comparison report from running both backends on the same input.
#[derive(Debug)]
pub struct OrchestrationComparison {
    /// Legacy backend result.
    pub legacy: OrchestrationResult,
    /// Bridge backend result.
    pub bridge: OrchestrationResult,
    /// Whether the ranking order matches exactly.
    pub ranking_match: bool,
    /// Maximum absolute score difference across corresponding positions.
    pub max_score_diff: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input() -> LegacySearchInput {
        LegacySearchInput {
            lexical_ranked: vec![(1, 10.0), (2, 9.0), (3, 8.0), (4, 7.0)],
            semantic_ranked: vec![(3, 0.99), (5, 0.95), (1, 0.90)],
            top_k: 5,
        }
    }

    // ── OrchestrationBackend ──────────────────────────────────────────

    #[test]
    fn backend_parse_legacy() {
        assert_eq!(
            OrchestrationBackend::parse("legacy"),
            OrchestrationBackend::Legacy
        );
        assert_eq!(
            OrchestrationBackend::parse("LEGACY"),
            OrchestrationBackend::Legacy
        );
        assert_eq!(
            OrchestrationBackend::parse(""),
            OrchestrationBackend::Legacy
        );
        assert_eq!(
            OrchestrationBackend::parse("unknown"),
            OrchestrationBackend::Legacy
        );
    }

    #[test]
    fn backend_parse_bridge() {
        assert_eq!(
            OrchestrationBackend::parse("bridge"),
            OrchestrationBackend::Bridge
        );
        assert_eq!(
            OrchestrationBackend::parse("BRIDGE"),
            OrchestrationBackend::Bridge
        );
        assert_eq!(
            OrchestrationBackend::parse("frankensearch"),
            OrchestrationBackend::Bridge
        );
        assert_eq!(
            OrchestrationBackend::parse("two_tier"),
            OrchestrationBackend::Bridge
        );
        assert_eq!(
            OrchestrationBackend::parse("twotier"),
            OrchestrationBackend::Bridge
        );
    }

    #[test]
    fn backend_as_str() {
        assert_eq!(OrchestrationBackend::Legacy.as_str(), "legacy");
        assert_eq!(OrchestrationBackend::Bridge.as_str(), "bridge");
    }

    #[test]
    fn backend_display() {
        assert_eq!(format!("{}", OrchestrationBackend::Legacy), "legacy");
        assert_eq!(format!("{}", OrchestrationBackend::Bridge), "bridge");
    }

    #[test]
    fn backend_serde_roundtrip() {
        for backend in [OrchestrationBackend::Legacy, OrchestrationBackend::Bridge] {
            let json = serde_json::to_string(&backend).unwrap();
            let back: OrchestrationBackend = serde_json::from_str(&json).unwrap();
            assert_eq!(backend, back);
        }
    }

    // ── OrchestratorConfig ────────────────────────────────────────────

    #[test]
    fn config_default() {
        let cfg = OrchestratorConfig::default();
        assert_eq!(cfg.mode, SearchModeConfig::Hybrid);
        assert_eq!(cfg.rrf_k, 60);
        assert!((cfg.alpha - 0.7).abs() < 1e-6);
        assert!((cfg.lexical_weight - 1.0).abs() < 1e-6);
        assert!((cfg.semantic_weight - 1.0).abs() < 1e-6);
        assert!(cfg.fallback_to_legacy);
        assert_eq!(cfg.embedder_dispatch, EmbedderDispatch::Legacy);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = OrchestratorConfig {
            backend: OrchestrationBackend::Bridge,
            mode: SearchModeConfig::Hybrid,
            rrf_k: 50,
            alpha: 0.6,
            lexical_weight: 0.8,
            semantic_weight: 1.2,
            fallback_to_legacy: false,
            embedder_dispatch: EmbedderDispatch::Managed,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: OrchestratorConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.backend, OrchestrationBackend::Bridge);
        assert_eq!(back.rrf_k, 50);
        assert_eq!(back.embedder_dispatch, EmbedderDispatch::Managed);
    }

    // ── SearchOrchestrator ────────────────────────────────────────────

    #[test]
    fn orchestrator_legacy_produces_results() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Legacy,
            ..Default::default()
        });
        let result = orch.fuse_ranked(&sample_input());
        assert!(!result.results.is_empty());
        assert_eq!(result.metrics.backend, "legacy");
        assert!(!result.metrics.fallback_occurred);
        assert_eq!(result.metrics.lexical_candidates, 4);
        assert_eq!(result.metrics.semantic_candidates, 3);
    }

    #[test]
    fn orchestrator_bridge_produces_results() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Bridge,
            ..Default::default()
        });
        let result = orch.fuse_ranked(&sample_input());
        assert!(!result.results.is_empty());
        assert_eq!(result.metrics.backend, "bridge");
    }

    #[test]
    fn orchestrator_legacy_and_bridge_agree_with_unit_weights() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            lexical_weight: 1.0,
            semantic_weight: 1.0,
            ..Default::default()
        });
        let comparison = orch.compare_backends(&sample_input());
        assert!(
            comparison.ranking_match,
            "unit-weight backends should produce identical rankings"
        );
        assert!(
            comparison.max_score_diff < 1e-5,
            "unit-weight backends should produce near-identical scores, got diff={}",
            comparison.max_score_diff
        );
    }

    #[test]
    fn orchestrator_deterministic() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Legacy,
            ..Default::default()
        });
        let input = sample_input();
        let r1 = orch.fuse_ranked(&input);
        let r2 = orch.fuse_ranked(&input);
        assert_eq!(r1.results.len(), r2.results.len());
        for (a, b) in r1.results.iter().zip(r2.results.iter()) {
            assert_eq!(a.id, b.id);
            assert!((a.score - b.score).abs() < 1e-10);
        }
    }

    #[test]
    fn orchestrator_lexical_only_mode() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Legacy,
            mode: SearchModeConfig::Lexical,
            ..Default::default()
        });
        let result = orch.fuse_ranked(&sample_input());
        let ids: Vec<u64> = result.results.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn orchestrator_semantic_only_mode() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Legacy,
            mode: SearchModeConfig::Semantic,
            ..Default::default()
        });
        let result = orch.fuse_ranked(&sample_input());
        let ids: Vec<u64> = result.results.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![3, 5, 1]);
    }

    #[test]
    fn orchestrator_hybrid_mode_includes_both() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Legacy,
            mode: SearchModeConfig::Hybrid,
            ..Default::default()
        });
        let result = orch.fuse_ranked(&sample_input());
        let ids: Vec<u64> = result.results.iter().map(|r| r.id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&3));
        assert!(ids.contains(&5));
    }

    #[test]
    fn orchestrator_empty_input() {
        let orch = SearchOrchestrator::new(OrchestratorConfig::default());
        let input = LegacySearchInput {
            lexical_ranked: vec![],
            semantic_ranked: vec![],
            top_k: 10,
        };
        let result = orch.fuse_ranked(&input);
        assert!(result.results.is_empty());
    }

    #[test]
    fn orchestrator_top_k_limiting() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Legacy,
            mode: SearchModeConfig::Lexical,
            ..Default::default()
        });
        let input = LegacySearchInput {
            lexical_ranked: vec![(1, 10.0), (2, 9.0), (3, 8.0), (4, 7.0)],
            semantic_ranked: vec![],
            top_k: 2,
        };
        let result = orch.fuse_ranked(&input);
        assert_eq!(result.results.len(), 2);
    }

    #[test]
    fn orchestrator_metrics_serde() {
        let metrics = OrchestrationMetrics {
            backend: "legacy".to_string(),
            effective_mode: "Hybrid".to_string(),
            fallback_occurred: false,
            fallback_reason: None,
            lexical_candidates: 100,
            semantic_candidates: 50,
            fusion: TwoTierMetrics::default(),
            embedder_dispatch: "Legacy".to_string(),
            embedder_availability: "none (legacy dispatch)".to_string(),
            embedder_tier_used: None,
            embedder_fallback: false,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let back: OrchestrationMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.backend, "legacy");
        assert_eq!(back.lexical_candidates, 100);
        assert_eq!(back.embedder_dispatch, "Legacy");
    }

    #[test]
    fn comparison_unit_weights_match() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            lexical_weight: 1.0,
            semantic_weight: 1.0,
            ..Default::default()
        });
        let comparison = orch.compare_backends(&sample_input());
        assert!(
            comparison.ranking_match,
            "unit-weight comparison should match"
        );
        assert!(comparison.max_score_diff < 1e-5);
        assert_eq!(comparison.legacy.metrics.backend, "legacy");
        assert_eq!(comparison.bridge.metrics.backend, "bridge");
    }

    #[test]
    fn bridge_fallback_on_panic_disabled() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Bridge,
            fallback_to_legacy: false,
            ..Default::default()
        });
        let result = orch.fuse_ranked(&sample_input());
        assert!(!result.results.is_empty());
        assert!(!result.metrics.fallback_occurred);
    }

    #[test]
    fn orchestrator_custom_rrf_k() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            rrf_k: 10,
            ..Default::default()
        });
        let result = orch.fuse_ranked(&sample_input());
        assert!(!result.results.is_empty());
    }

    #[test]
    fn orchestrator_custom_weights_affect_ranking() {
        let input = LegacySearchInput {
            lexical_ranked: vec![(100, 1.0), (200, 0.9)],
            semantic_ranked: vec![(300, 0.95), (400, 0.90)],
            top_k: 4,
        };

        let lex_biased = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Legacy,
            lexical_weight: 10.0,
            semantic_weight: 0.1,
            ..Default::default()
        });
        let result = lex_biased.fuse_ranked(&input);
        assert!(!result.results.is_empty());
        assert!(
            result.results[0].id == 100 || result.results[0].id == 200,
            "lexical bias should promote lexical items, got id={}",
            result.results[0].id,
        );

        let sem_biased = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Legacy,
            lexical_weight: 0.1,
            semantic_weight: 10.0,
            ..Default::default()
        });
        let result = sem_biased.fuse_ranked(&input);
        assert!(
            result.results[0].id == 300 || result.results[0].id == 400,
            "semantic bias should promote semantic items, got id={}",
            result.results[0].id,
        );
    }

    #[test]
    fn orchestrator_from_env_defaults_to_legacy() {
        let orch = SearchOrchestrator::from_env();
        assert_eq!(orch.backend(), OrchestrationBackend::Legacy);
    }

    #[test]
    fn orchestrator_debug_impl() {
        let orch = SearchOrchestrator::from_env();
        let debug = format!("{:?}", orch);
        assert!(debug.contains("SearchOrchestrator"));
        assert!(debug.contains("config"));
    }

    // ── B3: EmbedderDispatch ──────────────────────────────────────────

    #[test]
    fn embedder_dispatch_default_is_legacy() {
        assert_eq!(EmbedderDispatch::default(), EmbedderDispatch::Legacy);
    }

    #[test]
    fn embedder_dispatch_serde_roundtrip() {
        for d in [EmbedderDispatch::Legacy, EmbedderDispatch::Managed] {
            let json = serde_json::to_string(&d).unwrap();
            let back: EmbedderDispatch = serde_json::from_str(&json).unwrap();
            assert_eq!(d, back);
        }
    }

    // ── B3: EmbedderAvailability ──────────────────────────────────────

    #[test]
    fn embedder_availability_is_degraded() {
        assert!(!EmbedderAvailability::Full.is_degraded());
        assert!(EmbedderAvailability::FastOnly.is_degraded());
        assert!(EmbedderAvailability::HashOnly.is_degraded());
        assert!(!EmbedderAvailability::None.is_degraded());
    }

    #[test]
    fn embedder_availability_display() {
        assert!(
            format!("{}", EmbedderAvailability::Full).contains("quality")
        );
        assert!(
            format!("{}", EmbedderAvailability::FastOnly).contains("degraded")
        );
        assert!(
            format!("{}", EmbedderAvailability::HashOnly).contains("minimal")
        );
        assert!(
            format!("{}", EmbedderAvailability::None).contains("legacy")
        );
    }

    #[test]
    fn embedder_availability_serde_roundtrip() {
        for a in [
            EmbedderAvailability::Full,
            EmbedderAvailability::FastOnly,
            EmbedderAvailability::HashOnly,
            EmbedderAvailability::None,
        ] {
            let json = serde_json::to_string(&a).unwrap();
            let back: EmbedderAvailability = serde_json::from_str(&json).unwrap();
            assert_eq!(a, back);
        }
    }

    // ── B3: ManagedEmbedderStack ──────────────────────────────────────

    #[test]
    fn managed_stack_hash_only() {
        let stack = ManagedEmbedderStack::hash_only();
        assert_eq!(stack.availability(), EmbedderAvailability::HashOnly);
        let (emb, tier) = stack.best_embedder();
        assert_eq!(tier, EmbedderTier::Hash);
        assert_eq!(emb.dimension(), 128); // default HashEmbedder
    }

    #[test]
    fn managed_stack_from_tiers_full() {
        let hash: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(64));
        let fast: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(128));
        let quality: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(256));
        let stack = ManagedEmbedderStack::from_tiers(Some(quality), Some(fast), hash);
        assert_eq!(stack.availability(), EmbedderAvailability::Full);
        let (_, tier) = stack.best_embedder();
        assert_eq!(tier, EmbedderTier::Quality);
    }

    #[test]
    fn managed_stack_from_tiers_fast_only() {
        let hash: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(64));
        let fast: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(128));
        let stack = ManagedEmbedderStack::from_tiers(None, Some(fast), hash);
        assert_eq!(stack.availability(), EmbedderAvailability::FastOnly);
        let (_, tier) = stack.best_embedder();
        assert_eq!(tier, EmbedderTier::Fast);
    }

    #[test]
    fn managed_stack_from_tiers_hash_only() {
        let hash: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(64));
        let stack = ManagedEmbedderStack::from_tiers(None, None, hash);
        assert_eq!(stack.availability(), EmbedderAvailability::HashOnly);
    }

    #[test]
    fn managed_stack_embedder_for_tier() {
        let hash: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(64));
        let fast: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(128));
        let stack = ManagedEmbedderStack::from_tiers(None, Some(fast), hash);

        assert!(stack.embedder_for_tier(EmbedderTier::Hash).is_some());
        assert!(stack.embedder_for_tier(EmbedderTier::Fast).is_some());
        assert!(stack.embedder_for_tier(EmbedderTier::Quality).is_none());
    }

    #[test]
    fn managed_stack_embed_with_fallback_hash_only() {
        let stack = ManagedEmbedderStack::hash_only();
        let (vec, tier) = stack.embed_with_fallback("hello world").unwrap();
        assert_eq!(tier, EmbedderTier::Hash);
        assert_eq!(vec.len(), 128);
    }

    #[test]
    fn managed_stack_embed_with_fallback_uses_best() {
        let hash: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(64));
        let fast: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(128));
        let quality: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(256));
        let stack = ManagedEmbedderStack::from_tiers(Some(quality), Some(fast), hash);
        let (vec, tier) = stack.embed_with_fallback("test query").unwrap();
        // Should use quality (first available)
        assert_eq!(tier, EmbedderTier::Quality);
        assert_eq!(vec.len(), 256);
    }

    #[test]
    fn managed_stack_best_info() {
        let stack = ManagedEmbedderStack::hash_only();
        let info = stack.best_info();
        assert!(info.name.contains("hash"));
        assert_eq!(info.tier, EmbedderTier::Hash);
    }

    #[test]
    fn managed_stack_debug() {
        let stack = ManagedEmbedderStack::hash_only();
        let dbg = format!("{:?}", stack);
        assert!(dbg.contains("ManagedEmbedderStack"));
        assert!(dbg.contains("HashOnly"));
    }

    // ── B3: Orchestrator embedder integration ─────────────────────────

    #[test]
    fn orchestrator_no_embedder_by_default() {
        let orch = SearchOrchestrator::new(OrchestratorConfig::default());
        assert_eq!(orch.embedder_availability(), EmbedderAvailability::None);
        assert!(orch.embed_query("test").is_none());
    }

    #[test]
    fn orchestrator_with_embedder_stack() {
        let orch = SearchOrchestrator::new(OrchestratorConfig::default())
            .with_embedder_stack(ManagedEmbedderStack::hash_only());
        assert_eq!(
            orch.embedder_availability(),
            EmbedderAvailability::HashOnly
        );
        assert_eq!(
            orch.config().embedder_dispatch,
            EmbedderDispatch::Managed
        );
    }

    #[test]
    fn orchestrator_embed_query_with_stack() {
        let orch = SearchOrchestrator::new(OrchestratorConfig::default())
            .with_embedder_stack(ManagedEmbedderStack::hash_only());
        let result = orch.embed_query("hello world");
        assert!(result.is_some());
        let (vec, tier) = result.unwrap().unwrap();
        assert_eq!(tier, EmbedderTier::Hash);
        assert_eq!(vec.len(), 128);
    }

    #[test]
    fn orchestrator_metrics_include_embedder_info() {
        let orch = SearchOrchestrator::new(OrchestratorConfig::default())
            .with_embedder_stack(ManagedEmbedderStack::hash_only());
        let result = orch.fuse_ranked(&sample_input());
        assert_eq!(result.metrics.embedder_dispatch, "Managed");
        assert!(result.metrics.embedder_availability.contains("minimal"));
        assert!(!result.metrics.embedder_fallback);
    }

    #[test]
    fn orchestrator_legacy_dispatch_metrics() {
        let orch = SearchOrchestrator::new(OrchestratorConfig::default());
        let result = orch.fuse_ranked(&sample_input());
        assert_eq!(result.metrics.embedder_dispatch, "Legacy");
        assert!(result.metrics.embedder_availability.contains("legacy"));
        assert!(result.metrics.embedder_tier_used.is_none());
    }

    #[test]
    fn orchestrator_bridge_with_embedder_stack() {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            backend: OrchestrationBackend::Bridge,
            ..Default::default()
        })
        .with_embedder_stack(ManagedEmbedderStack::hash_only());
        let result = orch.fuse_ranked(&sample_input());
        assert_eq!(result.metrics.backend, "bridge");
        assert_eq!(result.metrics.embedder_dispatch, "Managed");
    }

    #[test]
    fn orchestrator_full_stack_availability() {
        let hash: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(64));
        let fast: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(128));
        let quality: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(256));
        let stack = ManagedEmbedderStack::from_tiers(Some(quality), Some(fast), hash);
        let orch = SearchOrchestrator::new(OrchestratorConfig::default())
            .with_embedder_stack(stack);
        assert_eq!(orch.embedder_availability(), EmbedderAvailability::Full);
        let result = orch.fuse_ranked(&sample_input());
        assert!(result.metrics.embedder_availability.contains("full"));
    }

    #[test]
    fn managed_stack_deterministic_embedding() {
        let stack = ManagedEmbedderStack::hash_only();
        let (v1, _) = stack.embed_with_fallback("test").unwrap();
        let (v2, _) = stack.embed_with_fallback("test").unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn embedder_dispatch_debug() {
        assert_eq!(format!("{:?}", EmbedderDispatch::Legacy), "Legacy");
        assert_eq!(format!("{:?}", EmbedderDispatch::Managed), "Managed");
    }

    #[test]
    fn embedder_availability_debug() {
        assert_eq!(format!("{:?}", EmbedderAvailability::Full), "Full");
        assert_eq!(format!("{:?}", EmbedderAvailability::None), "None");
    }
}
