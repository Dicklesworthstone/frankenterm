//! Compatibility facade for frankensearch migration (ft-dr6zv.1.3.C1).
//!
//! `SearchFacade` presents the same API surface as `HybridSearchService`
//! but can internally route to either the legacy direct path or the
//! `SearchOrchestrator` bridge path. This allows incremental migration
//! without breaking existing callers.
//!
//! # Routing modes
//!
//! - **Legacy** (default): delegates directly to `HybridSearchService::fuse()`.
//! - **Orchestrated**: routes through `SearchOrchestrator` with full B1-B8 dispatch.
//! - **Shadow**: runs both paths, compares results, returns legacy output.
//!   Logs discrepancies but never affects production behaviour.

use serde::{Deserialize, Serialize};

use super::hybrid_search::{
    FusedResult, FusionBackend, HybridSearchService, SearchMode, kendall_tau,
};
use super::orchestrator::{
    LegacySearchInput, OrchestratorConfig, SearchModeConfig, SearchOrchestrator,
};

// ---------------------------------------------------------------------------
// Routing enum
// ---------------------------------------------------------------------------

/// Facade routing strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FacadeRouting {
    /// Route directly through `HybridSearchService` (current production path).
    #[default]
    Legacy,
    /// Route through `SearchOrchestrator` (new path).
    Orchestrated,
    /// Shadow mode: run both paths, compare, return legacy results.
    Shadow,
}

impl FacadeRouting {
    /// Parse a routing selector string.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "orchestrated" | "bridge" | "new" | "orchestrator" => Self::Orchestrated,
            "shadow" | "compare" | "validate" => Self::Shadow,
            _ => Self::Legacy,
        }
    }

    /// Canonical string for this routing.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Orchestrated => "orchestrated",
            Self::Shadow => "shadow",
        }
    }
}

impl std::fmt::Display for FacadeRouting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

fn default_shadow_score_threshold() -> f32 {
    1e-4
}
fn default_shadow_tau_threshold() -> f32 {
    0.95
}

/// Configuration for the search facade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FacadeConfig {
    /// Routing strategy.
    pub routing: FacadeRouting,
    /// Underlying orchestrator config (used when routing != Legacy).
    pub orchestrator: OrchestratorConfig,
    /// Maximum acceptable score divergence in shadow mode before warning.
    #[serde(default = "default_shadow_score_threshold")]
    pub shadow_score_threshold: f32,
    /// Minimum acceptable Kendall tau correlation in shadow mode.
    #[serde(default = "default_shadow_tau_threshold")]
    pub shadow_tau_threshold: f32,
}

impl Default for FacadeConfig {
    fn default() -> Self {
        Self {
            routing: FacadeRouting::Legacy,
            orchestrator: OrchestratorConfig::default(),
            shadow_score_threshold: default_shadow_score_threshold(),
            shadow_tau_threshold: default_shadow_tau_threshold(),
        }
    }
}

// ---------------------------------------------------------------------------
// Shadow comparison
// ---------------------------------------------------------------------------

/// Shadow-mode comparison between legacy and orchestrated paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowComparison {
    /// Whether rankings matched exactly (same IDs in same order).
    pub ranking_match: bool,
    /// Maximum absolute score difference across common IDs.
    pub max_score_diff: f32,
    /// Kendall tau rank correlation between the two rankings.
    pub kendall_tau: f32,
    /// Whether the comparison passed configured thresholds.
    pub passed: bool,
    /// Reason for failure (if any).
    pub failure_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Facade result
// ---------------------------------------------------------------------------

/// Result from facade-routed search, including diagnostics.
#[derive(Debug, Clone)]
pub struct FacadeResult {
    /// Fused ranked results (same shape as `HybridSearchService` output).
    pub results: Vec<FusedResult>,
    /// Which routing path was used.
    pub routing_used: FacadeRouting,
    /// Shadow-mode comparison (only populated in Shadow mode).
    pub shadow_comparison: Option<ShadowComparison>,
}

// ---------------------------------------------------------------------------
// SearchFacade
// ---------------------------------------------------------------------------

/// Compatibility facade wrapping `HybridSearchService` + `SearchOrchestrator`.
///
/// Presents the exact same builder API as `HybridSearchService` so callers
/// can swap in `SearchFacade` without changing any call sites.
pub struct SearchFacade {
    legacy: HybridSearchService,
    config: FacadeConfig,
}

impl SearchFacade {
    /// Create a facade with default Legacy routing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            legacy: HybridSearchService::new(),
            config: FacadeConfig::default(),
        }
    }

    /// Create a facade with explicit configuration.
    #[must_use]
    pub fn with_config(config: FacadeConfig) -> Self {
        Self {
            legacy: HybridSearchService::new(),
            config,
        }
    }

    /// Create a facade from environment.
    ///
    /// Reads `FT_SEARCH_FACADE` for routing: `legacy` (default), `orchestrated`,
    /// or `shadow`.
    #[must_use]
    pub fn from_env() -> Self {
        let routing = std::env::var("FT_SEARCH_FACADE")
            .map(|v| FacadeRouting::parse(&v))
            .unwrap_or(FacadeRouting::Legacy);
        Self::with_config(FacadeConfig {
            routing,
            ..FacadeConfig::default()
        })
    }

    // -- Builder methods (delegate to inner HybridSearchService) --

    #[must_use]
    pub fn with_rrf_k(mut self, k: u32) -> Self {
        self.legacy = self.legacy.with_rrf_k(k);
        self
    }

    #[must_use]
    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.legacy = self.legacy.with_alpha(alpha);
        self
    }

    #[must_use]
    pub fn with_mode(mut self, mode: SearchMode) -> Self {
        self.legacy = self.legacy.with_mode(mode);
        self
    }

    #[must_use]
    pub fn with_fusion_backend(mut self, fb: FusionBackend) -> Self {
        self.legacy = self.legacy.with_fusion_backend(fb);
        self
    }

    #[must_use]
    pub fn with_rrf_weights(mut self, lexical: f32, semantic: f32) -> Self {
        self.legacy = self.legacy.with_rrf_weights(lexical, semantic);
        self
    }

    // -- Accessors --

    #[must_use]
    pub fn mode(&self) -> SearchMode {
        self.legacy.mode()
    }

    #[must_use]
    pub fn rrf_k(&self) -> u32 {
        self.legacy.rrf_k()
    }

    #[must_use]
    pub fn alpha(&self) -> f32 {
        self.legacy.alpha()
    }

    #[must_use]
    pub fn lexical_weight(&self) -> f32 {
        self.legacy.lexical_weight()
    }

    #[must_use]
    pub fn semantic_weight(&self) -> f32 {
        self.legacy.semantic_weight()
    }

    #[must_use]
    pub fn fusion_backend(&self) -> FusionBackend {
        self.legacy.fusion_backend()
    }

    #[must_use]
    pub fn routing(&self) -> FacadeRouting {
        self.config.routing
    }

    // -- Fuse API --

    /// Fuse ranked results — API-compatible with `HybridSearchService::fuse()`.
    pub fn fuse(
        &self,
        lexical: &[(u64, f32)],
        semantic: &[(u64, f32)],
        top_k: usize,
    ) -> Vec<FusedResult> {
        self.fuse_with_metrics(lexical, semantic, top_k).results
    }

    /// Fuse ranked results, returning both results and facade diagnostics.
    pub fn fuse_with_metrics(
        &self,
        lexical: &[(u64, f32)],
        semantic: &[(u64, f32)],
        top_k: usize,
    ) -> FacadeResult {
        match self.config.routing {
            FacadeRouting::Legacy => FacadeResult {
                results: self.legacy.fuse(lexical, semantic, top_k),
                routing_used: FacadeRouting::Legacy,
                shadow_comparison: None,
            },
            FacadeRouting::Orchestrated => {
                let orch = self.build_orchestrator();
                let input = LegacySearchInput {
                    lexical_ranked: lexical.to_vec(),
                    semantic_ranked: semantic.to_vec(),
                    top_k,
                };
                let result = orch.fuse_ranked(&input);
                FacadeResult {
                    results: result.results,
                    routing_used: FacadeRouting::Orchestrated,
                    shadow_comparison: None,
                }
            }
            FacadeRouting::Shadow => {
                let legacy_results = self.legacy.fuse(lexical, semantic, top_k);

                let orch = self.build_orchestrator();
                let input = LegacySearchInput {
                    lexical_ranked: lexical.to_vec(),
                    semantic_ranked: semantic.to_vec(),
                    top_k,
                };
                let orch_result = orch.fuse_ranked(&input);
                let comparison = self.compare_results(&legacy_results, &orch_result.results);

                if !comparison.passed {
                    tracing::warn!(
                        kendall_tau = comparison.kendall_tau,
                        max_score_diff = comparison.max_score_diff,
                        reason = ?comparison.failure_reason,
                        "search facade shadow comparison failed"
                    );
                }

                FacadeResult {
                    results: legacy_results,
                    routing_used: FacadeRouting::Shadow,
                    shadow_comparison: Some(comparison),
                }
            }
        }
    }

    // -- Internals --

    fn build_orchestrator(&self) -> SearchOrchestrator {
        let mut config = self.config.orchestrator.clone();
        config.mode = SearchModeConfig::from(self.legacy.mode());
        config.rrf_k = self.legacy.rrf_k();
        config.alpha = self.legacy.alpha();
        config.lexical_weight = self.legacy.lexical_weight();
        config.semantic_weight = self.legacy.semantic_weight();
        SearchOrchestrator::new(config)
    }

    fn compare_results(
        &self,
        legacy: &[FusedResult],
        orchestrated: &[FusedResult],
    ) -> ShadowComparison {
        let legacy_ids: Vec<u64> = legacy.iter().map(|r| r.id).collect();
        let orch_ids: Vec<u64> = orchestrated.iter().map(|r| r.id).collect();

        let ranking_match = legacy_ids == orch_ids;

        // Max score diff for common IDs.
        let mut max_score_diff: f32 = 0.0;
        let legacy_scores: std::collections::HashMap<u64, f32> =
            legacy.iter().map(|r| (r.id, r.score)).collect();
        for r in orchestrated {
            if let Some(&leg_score) = legacy_scores.get(&r.id) {
                let diff = (leg_score - r.score).abs();
                if diff > max_score_diff {
                    max_score_diff = diff;
                }
            }
        }

        let tau = kendall_tau(&legacy_ids, &orch_ids);

        let mut failure_reason = None;
        let mut passed = true;

        if tau < self.config.shadow_tau_threshold && !legacy_ids.is_empty() && !orch_ids.is_empty()
        {
            passed = false;
            failure_reason = Some(format!(
                "kendall tau {} below threshold {}",
                tau, self.config.shadow_tau_threshold
            ));
        }

        if max_score_diff > self.config.shadow_score_threshold {
            passed = false;
            let reason = format!(
                "max score diff {} exceeds threshold {}",
                max_score_diff, self.config.shadow_score_threshold
            );
            failure_reason = Some(match failure_reason {
                Some(prev) => format!("{prev}; {reason}"),
                None => reason,
            });
        }

        ShadowComparison {
            ranking_match,
            max_score_diff,
            kendall_tau: tau,
            passed,
            failure_reason,
        }
    }
}

impl Default for SearchFacade {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Routing parse/display --

    #[test]
    fn routing_parse_legacy() {
        assert_eq!(FacadeRouting::parse("legacy"), FacadeRouting::Legacy);
        assert_eq!(FacadeRouting::parse("unknown"), FacadeRouting::Legacy);
        assert_eq!(FacadeRouting::parse(""), FacadeRouting::Legacy);
    }

    #[test]
    fn routing_parse_orchestrated() {
        for s in &["orchestrated", "bridge", "new", "orchestrator"] {
            assert_eq!(FacadeRouting::parse(s), FacadeRouting::Orchestrated);
        }
    }

    #[test]
    fn routing_parse_shadow() {
        for s in &["shadow", "compare", "validate"] {
            assert_eq!(FacadeRouting::parse(s), FacadeRouting::Shadow);
        }
    }

    #[test]
    fn routing_as_str_roundtrip() {
        for r in &[
            FacadeRouting::Legacy,
            FacadeRouting::Orchestrated,
            FacadeRouting::Shadow,
        ] {
            assert_eq!(FacadeRouting::parse(r.as_str()), *r);
        }
    }

    #[test]
    fn routing_display() {
        assert_eq!(format!("{}", FacadeRouting::Legacy), "legacy");
        assert_eq!(format!("{}", FacadeRouting::Shadow), "shadow");
    }

    #[test]
    fn routing_serde_roundtrip() {
        for r in &[
            FacadeRouting::Legacy,
            FacadeRouting::Orchestrated,
            FacadeRouting::Shadow,
        ] {
            let json = serde_json::to_string(r).unwrap();
            let parsed: FacadeRouting = serde_json::from_str(&json).unwrap();
            assert_eq!(*r, parsed);
        }
    }

    #[test]
    fn config_default() {
        let cfg = FacadeConfig::default();
        assert_eq!(cfg.routing, FacadeRouting::Legacy);
        assert!((cfg.shadow_score_threshold - 1e-4).abs() < 1e-6);
        assert!((cfg.shadow_tau_threshold - 0.95).abs() < 1e-6);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = FacadeConfig {
            routing: FacadeRouting::Shadow,
            orchestrator: OrchestratorConfig::default(),
            shadow_score_threshold: 0.01,
            shadow_tau_threshold: 0.8,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: FacadeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.routing, FacadeRouting::Shadow);
        assert!((parsed.shadow_score_threshold - 0.01).abs() < 1e-6);
    }

    // -- Facade construction --

    #[test]
    fn facade_default_is_legacy() {
        let f = SearchFacade::new();
        assert_eq!(f.routing(), FacadeRouting::Legacy);
    }

    #[test]
    fn facade_default_trait() {
        let f = SearchFacade::default();
        assert_eq!(f.routing(), FacadeRouting::Legacy);
    }

    // -- Builder delegation --

    #[test]
    fn facade_builder_rrf_k() {
        let f = SearchFacade::new().with_rrf_k(42);
        assert_eq!(f.rrf_k(), 42);
    }

    #[test]
    fn facade_builder_alpha() {
        let f = SearchFacade::new().with_alpha(0.5);
        assert!((f.alpha() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn facade_builder_mode() {
        let f = SearchFacade::new().with_mode(SearchMode::Lexical);
        assert_eq!(f.mode(), SearchMode::Lexical);
    }

    #[test]
    fn facade_builder_fusion_backend() {
        let f = SearchFacade::new().with_fusion_backend(FusionBackend::FrankenSearchRrf);
        assert_eq!(f.fusion_backend(), FusionBackend::FrankenSearchRrf);
    }

    #[test]
    fn facade_builder_weights() {
        let f = SearchFacade::new().with_rrf_weights(0.3, 0.7);
        assert!((f.lexical_weight() - 0.3).abs() < 1e-6);
        assert!((f.semantic_weight() - 0.7).abs() < 1e-6);
    }

    // -- Fusion tests --

    fn sample_lexical() -> Vec<(u64, f32)> {
        vec![(1, 10.0), (2, 8.0), (3, 6.0), (4, 4.0)]
    }

    fn sample_semantic() -> Vec<(u64, f32)> {
        vec![(2, 0.9), (1, 0.8), (5, 0.7), (3, 0.6)]
    }

    #[test]
    fn facade_legacy_matches_direct() {
        let direct = HybridSearchService::new()
            .with_rrf_k(60)
            .with_rrf_weights(1.0, 1.0)
            .fuse(&sample_lexical(), &sample_semantic(), 10);

        let facade = SearchFacade::new()
            .with_rrf_k(60)
            .with_rrf_weights(1.0, 1.0)
            .fuse(&sample_lexical(), &sample_semantic(), 10);

        assert_eq!(direct.len(), facade.len());
        for (d, f) in direct.iter().zip(facade.iter()) {
            assert_eq!(d.id, f.id);
            assert!((d.score - f.score).abs() < 1e-6);
        }
    }

    #[test]
    fn facade_orchestrated_produces_results() {
        let config = FacadeConfig {
            routing: FacadeRouting::Orchestrated,
            ..FacadeConfig::default()
        };
        let facade = SearchFacade::with_config(config)
            .with_rrf_k(60)
            .with_rrf_weights(1.0, 1.0);

        let results = facade.fuse(&sample_lexical(), &sample_semantic(), 10);
        assert!(!results.is_empty());
    }

    #[test]
    fn facade_shadow_returns_legacy_results() {
        let config = FacadeConfig {
            routing: FacadeRouting::Shadow,
            ..FacadeConfig::default()
        };
        let facade = SearchFacade::with_config(config)
            .with_rrf_k(60)
            .with_rrf_weights(1.0, 1.0);

        let legacy = HybridSearchService::new()
            .with_rrf_k(60)
            .with_rrf_weights(1.0, 1.0)
            .fuse(&sample_lexical(), &sample_semantic(), 10);

        let shadow = facade.fuse(&sample_lexical(), &sample_semantic(), 10);

        assert_eq!(legacy.len(), shadow.len());
        for (l, s) in legacy.iter().zip(shadow.iter()) {
            assert_eq!(l.id, s.id);
            assert!((l.score - s.score).abs() < 1e-6);
        }
    }

    #[test]
    fn facade_shadow_has_comparison() {
        let config = FacadeConfig {
            routing: FacadeRouting::Shadow,
            ..FacadeConfig::default()
        };
        let facade = SearchFacade::with_config(config);

        let result = facade.fuse_with_metrics(&sample_lexical(), &sample_semantic(), 10);
        assert_eq!(result.routing_used, FacadeRouting::Shadow);
        assert!(result.shadow_comparison.is_some());
    }

    #[test]
    fn facade_shadow_comparison_passes_default_config() {
        let config = FacadeConfig {
            routing: FacadeRouting::Shadow,
            shadow_score_threshold: 1.0, // generous
            shadow_tau_threshold: -1.0,  // generous
            ..FacadeConfig::default()
        };
        let facade = SearchFacade::with_config(config);
        let result = facade.fuse_with_metrics(&sample_lexical(), &sample_semantic(), 10);
        let cmp = result.shadow_comparison.unwrap();
        assert!(
            cmp.passed,
            "comparison should pass with generous thresholds"
        );
    }

    #[test]
    fn facade_legacy_no_comparison() {
        let facade = SearchFacade::new();
        let result = facade.fuse_with_metrics(&sample_lexical(), &sample_semantic(), 10);
        assert_eq!(result.routing_used, FacadeRouting::Legacy);
        assert!(result.shadow_comparison.is_none());
    }

    #[test]
    fn facade_orchestrated_no_comparison() {
        let config = FacadeConfig {
            routing: FacadeRouting::Orchestrated,
            ..FacadeConfig::default()
        };
        let facade = SearchFacade::with_config(config);
        let result = facade.fuse_with_metrics(&sample_lexical(), &sample_semantic(), 10);
        assert_eq!(result.routing_used, FacadeRouting::Orchestrated);
        assert!(result.shadow_comparison.is_none());
    }

    #[test]
    fn facade_empty_inputs() {
        let facade = SearchFacade::new();
        let results = facade.fuse(&[], &[], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn facade_empty_inputs_orchestrated() {
        let config = FacadeConfig {
            routing: FacadeRouting::Orchestrated,
            ..FacadeConfig::default()
        };
        let facade = SearchFacade::with_config(config);
        let results = facade.fuse(&[], &[], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn facade_empty_inputs_shadow() {
        let config = FacadeConfig {
            routing: FacadeRouting::Shadow,
            ..FacadeConfig::default()
        };
        let facade = SearchFacade::with_config(config);
        let result = facade.fuse_with_metrics(&[], &[], 10);
        assert!(result.results.is_empty());
        let cmp = result.shadow_comparison.unwrap();
        assert!(cmp.passed);
        assert!(cmp.ranking_match);
    }

    #[test]
    fn facade_lexical_only_mode() {
        let facade = SearchFacade::new().with_mode(SearchMode::Lexical);
        let results = facade.fuse(&sample_lexical(), &sample_semantic(), 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, 1);
        assert_eq!(results[1].id, 2);
    }

    #[test]
    fn facade_semantic_only_mode() {
        let facade = SearchFacade::new().with_mode(SearchMode::Semantic);
        let results = facade.fuse(&sample_lexical(), &sample_semantic(), 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, 2);
        assert_eq!(results[1].id, 1);
    }

    #[test]
    fn facade_top_k_limits() {
        let facade = SearchFacade::new();
        let results = facade.fuse(&sample_lexical(), &sample_semantic(), 2);
        assert!(results.len() <= 2);
    }

    #[test]
    fn facade_results_sorted_descending() {
        let facade = SearchFacade::new();
        let results = facade.fuse(&sample_lexical(), &sample_semantic(), 10);
        for window in results.windows(2) {
            assert!(
                window[0].score >= window[1].score
                    || (window[0].score - window[1].score).abs() < 1e-8,
                "results should be sorted by score descending"
            );
        }
    }

    #[test]
    fn facade_orchestrated_results_sorted() {
        let config = FacadeConfig {
            routing: FacadeRouting::Orchestrated,
            ..FacadeConfig::default()
        };
        let facade = SearchFacade::with_config(config);
        let results = facade.fuse(&sample_lexical(), &sample_semantic(), 10);
        for window in results.windows(2) {
            assert!(
                window[0].score >= window[1].score
                    || (window[0].score - window[1].score).abs() < 1e-8,
            );
        }
    }

    #[test]
    fn shadow_comparison_detects_exact_match() {
        let results = vec![
            FusedResult {
                id: 1,
                score: 1.0,
                lexical_rank: Some(0),
                semantic_rank: Some(1),
            },
            FusedResult {
                id: 2,
                score: 0.5,
                lexical_rank: Some(1),
                semantic_rank: Some(0),
            },
        ];
        let facade = SearchFacade::new();
        let cmp = facade.compare_results(&results, &results);
        assert!(cmp.ranking_match);
        assert!(cmp.max_score_diff < 1e-6);
        assert!(cmp.passed);
    }

    #[test]
    fn shadow_comparison_detects_reorder() {
        let legacy = vec![
            FusedResult {
                id: 1,
                score: 1.0,
                lexical_rank: Some(0),
                semantic_rank: None,
            },
            FusedResult {
                id: 2,
                score: 0.5,
                lexical_rank: Some(1),
                semantic_rank: None,
            },
        ];
        let reordered = vec![
            FusedResult {
                id: 2,
                score: 0.5,
                lexical_rank: Some(1),
                semantic_rank: None,
            },
            FusedResult {
                id: 1,
                score: 1.0,
                lexical_rank: Some(0),
                semantic_rank: None,
            },
        ];
        let facade = SearchFacade::new();
        let cmp = facade.compare_results(&legacy, &reordered);
        assert!(!cmp.ranking_match);
    }
}
