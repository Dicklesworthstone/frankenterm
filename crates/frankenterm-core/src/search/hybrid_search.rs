//! Reciprocal Rank Fusion, two-tier blending, and hybrid search orchestration.

use std::collections::{HashMap, HashSet};

/// Search mode selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Lexical only (BM25).
    Lexical,
    /// Semantic only (embedding similarity).
    Semantic,
    /// Hybrid: fuse lexical + semantic via RRF, then two-tier blend.
    Hybrid,
}

/// Fusion backend selector for hybrid ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FusionBackend {
    /// Use `frankensearch` RRF fusion.
    FrankenSearchRrf,
}

impl FusionBackend {
    /// Parse a fusion backend selector string.
    ///
    /// Supported values:
    /// - `frankensearch`, `frankensearch_rrf`, `frankensearch-rrf`
    /// - legacy aliases (`legacy`, empty, unknown) are normalized to FrankenSearch.
    #[must_use]
    pub fn parse(_raw: &str) -> Self {
        // All inputs normalize to FrankenSearchRrf (legacy backends removed).
        Self::FrankenSearchRrf
    }

    /// Canonical backend selector string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        "frankensearch_rrf"
    }

    /// Resolve backend from `FT_SEARCH_FUSION_BACKEND`.
    ///
    /// Supported values:
    /// - `frankensearch`, `frankensearch_rrf`, `frankensearch-rrf`
    /// - unset/unknown values default to `frankensearch_rrf`
    #[must_use]
    pub fn from_env() -> Self {
        let Ok(raw) = std::env::var("FT_SEARCH_FUSION_BACKEND") else {
            return Self::FrankenSearchRrf;
        };
        Self::parse(&raw)
    }
}

/// A fused search result with combined score.
#[derive(Debug, Clone)]
pub struct FusedResult {
    pub id: u64,
    pub score: f32,
    pub lexical_rank: Option<usize>,
    pub semantic_rank: Option<usize>,
}

fn rrf_component_score(rank: usize, k: u32, weight: f32) -> f32 {
    if weight <= 0.0 {
        return 0.0;
    }
    weight / (k as f32 + rank as f32 + 1.0)
}

fn dedupe_ranked_hits(items: &[(u64, f32)]) -> Vec<(u64, f32)> {
    let mut seen = HashSet::with_capacity(items.len());
    let mut deduped = Vec::with_capacity(items.len());
    for &(id, score) in items {
        if seen.insert(id) {
            deduped.push((id, score));
        }
    }
    deduped
}

/// Metrics from two-tier blending.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TwoTierMetrics {
    pub tier1_count: usize,
    pub tier2_count: usize,
    pub overlap_count: usize,
    pub rank_correlation: f32,
}

/// Reciprocal Rank Fusion with parameter k (default 60).
///
/// Given multiple ranked lists of (id, score), produce a single fused ranking.
/// RRF score = sum(1 / (k + rank_i)) for each list where the item appears.
pub fn rrf_fuse(lexical: &[(u64, f32)], semantic: &[(u64, f32)], k: u32) -> Vec<FusedResult> {
    rrf_fuse_weighted(lexical, semantic, k, 1.0, 1.0)
}

/// Weighted Reciprocal Rank Fusion.
///
/// `lexical_weight` and `semantic_weight` scale the contribution of each lane.
pub fn rrf_fuse_weighted(
    lexical: &[(u64, f32)],
    semantic: &[(u64, f32)],
    k: u32,
    lexical_weight: f32,
    semantic_weight: f32,
) -> Vec<FusedResult> {
    let lexical = dedupe_ranked_hits(lexical);
    let semantic = dedupe_ranked_hits(semantic);
    let mut scores: HashMap<u64, (f32, Option<usize>, Option<usize>)> = HashMap::new();

    for (rank, &(id, _score)) in lexical.iter().enumerate() {
        let entry = scores.entry(id).or_insert((0.0, None, None));
        entry.0 += rrf_component_score(rank, k, lexical_weight);
        entry.1 = Some(rank);
    }

    for (rank, &(id, _score)) in semantic.iter().enumerate() {
        let entry = scores.entry(id).or_insert((0.0, None, None));
        entry.0 += rrf_component_score(rank, k, semantic_weight);
        entry.2 = Some(rank);
    }

    let mut results: Vec<FusedResult> = scores
        .into_iter()
        .map(|(id, (score, lex_rank, sem_rank))| FusedResult {
            id,
            score,
            lexical_rank: lex_rank,
            semantic_rank: sem_rank,
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    results
}

/// Weighted RRF fusion delegated to frankensearch.
///
/// Uses frankensearch's `rrf_fuse()` for rank assignment (which lane each item
/// came from and its rank within that lane), then recomputes final scores using
/// the weight-adjusted RRF formula: `weight / (k + rank + 1)` per lane.
///
/// This preserves frankensearch's deduplication and rank-assignment logic while
/// adding weight support that frankensearch's raw `rrf_fuse()` does not expose.
///
/// When the `frankensearch` feature is disabled, falls back to the local
/// `rrf_fuse_weighted()` implementation.
fn rrf_fuse_with_frankensearch(
    lexical: &[(u64, f32)],
    semantic: &[(u64, f32)],
    k: u32,
    lexical_weight: f32,
    semantic_weight: f32,
) -> Vec<FusedResult> {
    let lexical = dedupe_ranked_hits(lexical);
    let semantic = dedupe_ranked_hits(semantic);
    #[cfg(not(feature = "frankensearch"))]
    {
        rrf_fuse_weighted(&lexical, &semantic, k, lexical_weight, semantic_weight)
    }

    #[cfg(feature = "frankensearch")]
    {
        let lexical_hits: Vec<frankensearch::ScoredResult> = lexical
            .iter()
            .map(|(id, score)| frankensearch::ScoredResult {
                doc_id: id.to_string(),
                score: *score,
                source: frankensearch::ScoreSource::Lexical,
                fast_score: None,
                quality_score: None,
                lexical_score: Some(*score),
                rerank_score: None,
                explanation: None,
                metadata: None,
            })
            .collect();

        let semantic_hits: Vec<frankensearch::VectorHit> = semantic
            .iter()
            .enumerate()
            .map(|(index, (id, score))| frankensearch::VectorHit {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                score: *score,
                doc_id: id.to_string(),
            })
            .collect();

        let config = frankensearch::RrfConfig { k: f64::from(k) };
        let limit = lexical_hits.len().saturating_add(semantic_hits.len());

        let mut results: Vec<FusedResult> =
            frankensearch::rrf_fuse(&lexical_hits, &semantic_hits, limit, 0, &config)
                .into_iter()
                .filter_map(|hit| {
                    hit.doc_id.parse::<u64>().ok().map(|id| {
                        // Recompute score with weight-adjusted RRF formula.
                        // frankensearch provides per-lane ranks; we apply per-lane
                        // weights to compute the final fused score.
                        let mut score = 0.0f32;
                        if let Some(r) = hit.lexical_rank {
                            score += rrf_component_score(r, k, lexical_weight);
                        }
                        if let Some(r) = hit.semantic_rank {
                            score += rrf_component_score(r, k, semantic_weight);
                        }
                        FusedResult {
                            id,
                            score,
                            lexical_rank: hit.lexical_rank,
                            semantic_rank: hit.semantic_rank,
                        }
                    })
                })
                .collect();

        // Re-sort by weighted score (weights may reorder vs unweighted).
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        results
    }
}

/// Two-tier blending: take top-n from tier1 (quality embedder) and fill remaining
/// from tier2 (hash embedder), deduplicating by id.
///
/// `alpha` controls the weight of tier1 scores (0.0 = all tier2, 1.0 = all tier1).
pub fn blend_two_tier(
    tier1: &[FusedResult],
    tier2: &[FusedResult],
    top_k: usize,
    alpha: f32,
) -> (Vec<FusedResult>, TwoTierMetrics) {
    let alpha = alpha.clamp(0.0, 1.0);

    let mut seen: HashSet<u64> = HashSet::new();
    let mut results = Vec::with_capacity(top_k);
    let mut metrics = TwoTierMetrics::default();

    // Count overlap
    let tier1_ids: std::collections::HashSet<u64> = tier1.iter().map(|r| r.id).collect();
    let tier2_ids: std::collections::HashSet<u64> = tier2.iter().map(|r| r.id).collect();
    metrics.overlap_count = tier1_ids.intersection(&tier2_ids).count();

    // Tier 1 first
    for r in tier1 {
        if results.len() >= top_k {
            break;
        }
        if seen.contains(&r.id) {
            continue;
        }
        seen.insert(r.id);
        results.push(FusedResult {
            id: r.id,
            score: r.score * alpha,
            lexical_rank: r.lexical_rank,
            semantic_rank: r.semantic_rank,
        });
        metrics.tier1_count += 1;
    }

    // Fill from tier 2
    for r in tier2 {
        if results.len() >= top_k {
            break;
        }
        if seen.contains(&r.id) {
            continue;
        }
        seen.insert(r.id);
        results.push(FusedResult {
            id: r.id,
            score: r.score * (1.0 - alpha),
            lexical_rank: r.lexical_rank,
            semantic_rank: r.semantic_rank,
        });
        metrics.tier2_count += 1;
    }

    (results, metrics)
}

/// Kendall's tau rank correlation coefficient between two rankings.
///
/// Rankings are given as slices of IDs in rank order. Returns a value in [-1, 1].
pub fn kendall_tau(ranking_a: &[u64], ranking_b: &[u64]) -> f32 {
    if ranking_a.is_empty() || ranking_b.is_empty() {
        return 0.0;
    }

    // Build rank maps
    let rank_a: HashMap<u64, usize> = ranking_a
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();
    let rank_b: HashMap<u64, usize> = ranking_b
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();

    // Only consider items present in both
    let common: Vec<u64> = ranking_a
        .iter()
        .copied()
        .filter(|id| rank_b.contains_key(id))
        .collect();

    let n = common.len();
    if n < 2 {
        return 0.0;
    }

    let mut concordant = 0i64;
    let mut discordant = 0i64;

    for i in 0..n {
        for j in (i + 1)..n {
            let a_i = rank_a[&common[i]];
            let a_j = rank_a[&common[j]];
            let b_i = rank_b[&common[i]];
            let b_j = rank_b[&common[j]];

            let a_order = (a_i as i64) - (a_j as i64);
            let b_order = (b_i as i64) - (b_j as i64);

            if a_order * b_order > 0 {
                concordant += 1;
            } else if a_order * b_order < 0 {
                discordant += 1;
            }
        }
    }

    let total = concordant + discordant;
    if total == 0 {
        return 0.0;
    }
    (concordant - discordant) as f32 / total as f32
}

/// Hybrid search service that orchestrates lexical and semantic retrieval.
pub struct HybridSearchService {
    rrf_k: u32,
    alpha: f32,
    mode: SearchMode,
    fusion_backend: FusionBackend,
    lexical_weight: f32,
    semantic_weight: f32,
}

impl HybridSearchService {
    pub fn new() -> Self {
        Self {
            rrf_k: 60,
            alpha: 0.7,
            mode: SearchMode::Hybrid,
            fusion_backend: FusionBackend::from_env(),
            lexical_weight: 1.0,
            semantic_weight: 1.0,
        }
    }

    #[must_use]
    pub fn with_rrf_k(mut self, k: u32) -> Self {
        self.rrf_k = k;
        self
    }

    #[must_use]
    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.alpha = alpha.clamp(0.0, 1.0);
        self
    }

    #[must_use]
    pub fn with_mode(mut self, mode: SearchMode) -> Self {
        self.mode = mode;
        self
    }

    #[must_use]
    pub fn with_fusion_backend(mut self, fusion_backend: FusionBackend) -> Self {
        self.fusion_backend = fusion_backend;
        self
    }

    #[must_use]
    pub fn with_rrf_weights(mut self, lexical_weight: f32, semantic_weight: f32) -> Self {
        self.lexical_weight = lexical_weight.max(0.0);
        self.semantic_weight = semantic_weight.max(0.0);
        self
    }

    pub fn mode(&self) -> SearchMode {
        self.mode
    }

    pub fn rrf_k(&self) -> u32 {
        self.rrf_k
    }

    pub fn alpha(&self) -> f32 {
        self.alpha
    }

    pub fn lexical_weight(&self) -> f32 {
        self.lexical_weight
    }

    pub fn fusion_backend(&self) -> FusionBackend {
        self.fusion_backend
    }

    pub fn semantic_weight(&self) -> f32 {
        self.semantic_weight
    }

    /// Fuse lexical and semantic results according to the configured mode.
    pub fn fuse(
        &self,
        lexical: &[(u64, f32)],
        semantic: &[(u64, f32)],
        top_k: usize,
    ) -> Vec<FusedResult> {
        match self.mode {
            SearchMode::Lexical => lexical
                .iter()
                .take(top_k)
                .enumerate()
                .map(|(rank, &(id, score))| FusedResult {
                    id,
                    score,
                    lexical_rank: Some(rank),
                    semantic_rank: None,
                })
                .collect(),
            SearchMode::Semantic => semantic
                .iter()
                .take(top_k)
                .enumerate()
                .map(|(rank, &(id, score))| FusedResult {
                    id,
                    score,
                    lexical_rank: None,
                    semantic_rank: Some(rank),
                })
                .collect(),
            SearchMode::Hybrid => {
                let fused = match self.fusion_backend {
                    FusionBackend::FrankenSearchRrf => {
                        // Delegate ranking to frankensearch with weight-aware scoring.
                        // frankensearch assigns per-lane ranks; we apply per-lane
                        // weights via the local RRF component score formula.
                        rrf_fuse_with_frankensearch(
                            lexical,
                            semantic,
                            self.rrf_k,
                            self.lexical_weight,
                            self.semantic_weight,
                        )
                    }
                };
                fused.into_iter().take(top_k).collect()
            }
        }
    }
}

impl Default for HybridSearchService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_fuse_basic() {
        let lexical = vec![(1, 10.0), (2, 8.0), (3, 6.0)];
        let semantic = vec![(2, 0.9), (1, 0.8), (4, 0.7)];
        let fused = rrf_fuse(&lexical, &semantic, 60);
        // items 1 and 2 appear in both lists, should have higher scores
        assert!(fused.len() >= 3);
        // top result should be id 1 or 2 (both in both lists)
        assert!(fused[0].id == 1 || fused[0].id == 2);
    }

    #[test]
    fn rrf_fuse_empty() {
        let fused = rrf_fuse(&[], &[], 60);
        assert!(fused.is_empty());
    }

    #[test]
    fn rrf_fuse_single_list() {
        let lexical = vec![(1, 10.0), (2, 8.0)];
        let fused = rrf_fuse(&lexical, &[], 60);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].id, 1);
    }

    #[test]
    fn rrf_maintains_both_ranks() {
        let lexical = vec![(1, 10.0)];
        let semantic = vec![(1, 0.9)];
        let fused = rrf_fuse(&lexical, &semantic, 60);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].lexical_rank, Some(0));
        assert_eq!(fused[0].semantic_rank, Some(0));
    }

    #[test]
    fn blend_two_tier_basic() {
        let tier1 = vec![
            FusedResult {
                id: 1,
                score: 1.0,
                lexical_rank: Some(0),
                semantic_rank: Some(0),
            },
            FusedResult {
                id: 2,
                score: 0.8,
                lexical_rank: Some(1),
                semantic_rank: None,
            },
        ];
        let tier2 = vec![
            FusedResult {
                id: 3,
                score: 0.9,
                lexical_rank: None,
                semantic_rank: Some(0),
            },
            FusedResult {
                id: 4,
                score: 0.7,
                lexical_rank: None,
                semantic_rank: Some(1),
            },
        ];
        let (results, metrics) = blend_two_tier(&tier1, &tier2, 3, 0.7);
        assert_eq!(results.len(), 3);
        assert_eq!(metrics.tier1_count, 2);
        assert_eq!(metrics.tier2_count, 1);
    }

    #[test]
    fn blend_deduplicates() {
        let tier1 = vec![FusedResult {
            id: 1,
            score: 1.0,
            lexical_rank: None,
            semantic_rank: None,
        }];
        let tier2 = vec![
            FusedResult {
                id: 1,
                score: 0.5,
                lexical_rank: None,
                semantic_rank: None,
            },
            FusedResult {
                id: 2,
                score: 0.4,
                lexical_rank: None,
                semantic_rank: None,
            },
        ];
        let (results, metrics) = blend_two_tier(&tier1, &tier2, 5, 0.5);
        // id=1 appears only once
        assert_eq!(results.len(), 2);
        assert_eq!(metrics.overlap_count, 1);
    }

    #[test]
    fn blend_alpha_zero() {
        let tier1 = vec![FusedResult {
            id: 1,
            score: 1.0,
            lexical_rank: None,
            semantic_rank: None,
        }];
        let tier2 = vec![FusedResult {
            id: 2,
            score: 0.5,
            lexical_rank: None,
            semantic_rank: None,
        }];
        let (results, _) = blend_two_tier(&tier1, &tier2, 10, 0.0);
        // alpha=0 means tier1 scores are zeroed, tier2 scores are full
        assert!(results[0].score.abs() < f32::EPSILON); // 1.0 * 0.0
        assert!((results[1].score - 0.5).abs() < f32::EPSILON); // 0.5 * 1.0
    }

    #[test]
    fn kendall_tau_identical() {
        let a = vec![1, 2, 3, 4, 5];
        let b = vec![1, 2, 3, 4, 5];
        let tau = kendall_tau(&a, &b);
        assert!((tau - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn kendall_tau_reversed() {
        let a = vec![1, 2, 3, 4, 5];
        let b = vec![5, 4, 3, 2, 1];
        let tau = kendall_tau(&a, &b);
        assert!((tau - (-1.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn kendall_tau_empty() {
        assert!(kendall_tau(&[], &[1, 2, 3]).abs() < f32::EPSILON);
        assert!(kendall_tau(&[1], &[]).abs() < f32::EPSILON);
    }

    #[test]
    fn kendall_tau_partial_overlap() {
        let a = vec![1, 2, 3];
        let b = vec![2, 3, 4]; // only 2, 3 in common
        let tau = kendall_tau(&a, &b);
        // 2 before 3 in both => concordant, tau = 1.0
        assert!((tau - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn kendall_tau_no_overlap() {
        let a = vec![1, 2];
        let b = vec![3, 4];
        let tau = kendall_tau(&a, &b);
        assert!(tau.abs() < f32::EPSILON);
    }

    #[test]
    fn hybrid_service_defaults() {
        let svc = HybridSearchService::new();
        assert_eq!(svc.rrf_k(), 60);
        assert!((svc.alpha() - 0.7).abs() < f32::EPSILON);
        assert_eq!(svc.mode(), SearchMode::Hybrid);
        assert_eq!(svc.fusion_backend(), FusionBackend::FrankenSearchRrf);
        assert!((svc.lexical_weight() - 1.0).abs() < f32::EPSILON);
        assert!((svc.semantic_weight() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn hybrid_service_lexical_mode() {
        let svc = HybridSearchService::new().with_mode(SearchMode::Lexical);
        let results = svc.fuse(&[(1, 10.0), (2, 8.0)], &[(3, 0.9)], 5);
        assert_eq!(results.len(), 2);
        // semantic results ignored in lexical mode
        assert!(results.iter().all(|r| r.semantic_rank.is_none()));
    }

    #[test]
    fn hybrid_service_semantic_mode() {
        let svc = HybridSearchService::new().with_mode(SearchMode::Semantic);
        let results = svc.fuse(&[(1, 10.0)], &[(3, 0.9), (4, 0.8)], 5);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.lexical_rank.is_none()));
    }

    #[test]
    fn hybrid_service_custom_k() {
        let svc = HybridSearchService::new().with_rrf_k(30);
        assert_eq!(svc.rrf_k(), 30);
    }

    #[test]
    fn hybrid_service_alpha_clamp() {
        let svc = HybridSearchService::new().with_alpha(1.5);
        assert!((svc.alpha() - 1.0).abs() < f32::EPSILON);
        let svc = HybridSearchService::new().with_alpha(-0.5);
        assert!(svc.alpha().abs() < f32::EPSILON);
    }

    #[test]
    fn rrf_score_decreases_with_rank() {
        let lexical = vec![(1, 10.0), (2, 8.0), (3, 6.0)];
        let fused = rrf_fuse(&lexical, &[], 60);
        assert!(fused[0].score > fused[1].score);
        assert!(fused[1].score > fused[2].score);
    }

    #[test]
    fn rrf_tie_breaks_by_id_for_determinism() {
        let lexical = vec![(2, 1.0)];
        let semantic = vec![(1, 1.0)];
        let fused = rrf_fuse(&lexical, &semantic, 60);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].id, 1);
        assert_eq!(fused[1].id, 2);
    }

    #[test]
    fn weighted_rrf_can_bias_lexical_lane() {
        let lexical = vec![(1, 1.0)];
        let semantic = vec![(2, 1.0)];
        let fused = HybridSearchService::new()
            .with_rrf_weights(2.0, 0.5)
            .fuse(&lexical, &semantic, 10);
        assert_eq!(fused[0].id, 1);
    }

    // -----------------------------------------------------------------------
    // Batch 11 — TopazBay wa-1u90p.7.1
    // -----------------------------------------------------------------------

    // ---- rrf_component_score direct tests ----

    #[test]
    fn rrf_component_score_zero_weight() {
        assert!(rrf_component_score(0, 60, 0.0).abs() < f32::EPSILON);
        assert!(rrf_component_score(5, 60, 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rrf_component_score_negative_weight() {
        assert!(rrf_component_score(0, 60, -1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rrf_component_score_normal() {
        // weight / (k + rank + 1) = 1.0 / (60 + 0 + 1) = 1/61
        let score = rrf_component_score(0, 60, 1.0);
        assert!((score - 1.0 / 61.0).abs() < 1e-6);
    }

    #[test]
    fn rrf_component_score_higher_rank_lower_score() {
        let s0 = rrf_component_score(0, 60, 1.0);
        let s5 = rrf_component_score(5, 60, 1.0);
        let s100 = rrf_component_score(100, 60, 1.0);
        assert!(s0 > s5);
        assert!(s5 > s100);
    }

    #[test]
    fn rrf_component_score_k_zero() {
        // weight / (0 + rank + 1) = 1.0 / (0 + 0 + 1) = 1.0
        let score = rrf_component_score(0, 0, 1.0);
        assert!((score - 1.0).abs() < 1e-6);
    }

    // ---- rrf_fuse_weighted edge cases ----

    #[test]
    fn rrf_fuse_weighted_zero_weights_returns_zeros() {
        let lexical = vec![(1, 10.0), (2, 8.0)];
        let semantic = vec![(3, 0.9)];
        let fused = rrf_fuse_weighted(&lexical, &semantic, 60, 0.0, 0.0);
        assert!(fused.iter().all(|r| r.score.abs() < f32::EPSILON));
    }

    #[test]
    fn rrf_fuse_weighted_semantic_only() {
        let lexical = vec![(1, 10.0)];
        let semantic = vec![(2, 0.9)];
        let fused = rrf_fuse_weighted(&lexical, &semantic, 60, 0.0, 1.0);
        // id=2 (semantic) should have higher score; id=1 (lexical zero weight) should be 0
        let r1 = fused.iter().find(|r| r.id == 1).unwrap();
        let r2 = fused.iter().find(|r| r.id == 2).unwrap();
        assert!(r1.score.abs() < f32::EPSILON);
        assert!(r2.score > 0.0);
    }

    #[test]
    fn rrf_fuse_duplicate_ids_in_same_list() {
        // Duplicate IDs should not receive multiple contributions in the same lane.
        // First rank wins; later duplicates are ignored.
        let lexical = vec![(1, 10.0), (1, 8.0)];
        let fused = rrf_fuse(&lexical, &[], 60);
        assert_eq!(fused.len(), 1);
        assert!((fused[0].score - (1.0 / 61.0)).abs() < 1e-6);
        assert_eq!(fused[0].lexical_rank, Some(0));
    }

    #[test]
    fn rrf_fuse_ignores_duplicate_ids_per_lane() {
        let lexical = vec![(10, 1.0), (10, 0.9), (20, 0.8)];
        let semantic = vec![(10, 0.7), (10, 0.6), (30, 0.5)];
        let fused = rrf_fuse(&lexical, &semantic, 60);

        let id10 = fused
            .iter()
            .find(|hit| hit.id == 10)
            .expect("id 10 present");
        let expected = rrf_component_score(0, 60, 1.0) + rrf_component_score(0, 60, 1.0);
        assert!((id10.score - expected).abs() < 1e-6);
        assert_eq!(id10.lexical_rank, Some(0));
        assert_eq!(id10.semantic_rank, Some(0));
    }

    // ---- blend_two_tier edge cases ----

    #[test]
    fn blend_alpha_one() {
        let tier1 = vec![FusedResult {
            id: 1,
            score: 1.0,
            lexical_rank: None,
            semantic_rank: None,
        }];
        let tier2 = vec![FusedResult {
            id: 2,
            score: 0.5,
            lexical_rank: None,
            semantic_rank: None,
        }];
        let (results, _) = blend_two_tier(&tier1, &tier2, 10, 1.0);
        // alpha=1.0: tier1 score * 1.0, tier2 score * 0.0
        assert!((results[0].score - 1.0).abs() < f32::EPSILON);
        assert!(results[1].score.abs() < f32::EPSILON);
    }

    #[test]
    fn blend_empty_tier1() {
        let tier2 = vec![
            FusedResult {
                id: 1,
                score: 0.9,
                lexical_rank: None,
                semantic_rank: None,
            },
            FusedResult {
                id: 2,
                score: 0.8,
                lexical_rank: None,
                semantic_rank: None,
            },
        ];
        let (results, metrics) = blend_two_tier(&[], &tier2, 5, 0.7);
        assert_eq!(results.len(), 2);
        assert_eq!(metrics.tier1_count, 0);
        assert_eq!(metrics.tier2_count, 2);
        assert_eq!(metrics.overlap_count, 0);
    }

    #[test]
    fn blend_empty_tier2() {
        let tier1 = vec![FusedResult {
            id: 1,
            score: 1.0,
            lexical_rank: Some(0),
            semantic_rank: Some(0),
        }];
        let (results, metrics) = blend_two_tier(&tier1, &[], 5, 0.7);
        assert_eq!(results.len(), 1);
        assert_eq!(metrics.tier1_count, 1);
        assert_eq!(metrics.tier2_count, 0);
    }

    #[test]
    fn blend_top_k_limits_output() {
        let tier1 = vec![
            FusedResult {
                id: 1,
                score: 1.0,
                lexical_rank: None,
                semantic_rank: None,
            },
            FusedResult {
                id: 2,
                score: 0.9,
                lexical_rank: None,
                semantic_rank: None,
            },
            FusedResult {
                id: 3,
                score: 0.8,
                lexical_rank: None,
                semantic_rank: None,
            },
        ];
        let (results, _) = blend_two_tier(&tier1, &[], 2, 0.7);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn blend_top_k_zero() {
        let tier1 = vec![FusedResult {
            id: 1,
            score: 1.0,
            lexical_rank: None,
            semantic_rank: None,
        }];
        let (results, _) = blend_two_tier(&tier1, &[], 0, 0.7);
        assert!(results.is_empty());
    }

    #[test]
    fn blend_alpha_clamped_above_one() {
        let tier1 = vec![FusedResult {
            id: 1,
            score: 1.0,
            lexical_rank: None,
            semantic_rank: None,
        }];
        let tier2 = vec![FusedResult {
            id: 2,
            score: 1.0,
            lexical_rank: None,
            semantic_rank: None,
        }];
        // alpha > 1.0 is clamped to 1.0
        let (results, _) = blend_two_tier(&tier1, &tier2, 10, 2.0);
        assert!((results[0].score - 1.0).abs() < f32::EPSILON);
        assert!(results[1].score.abs() < f32::EPSILON);
    }

    // ---- kendall_tau edge cases ----

    #[test]
    fn kendall_tau_single_element() {
        // Single common element means n < 2, returns 0.0
        assert!(kendall_tau(&[1], &[1]).abs() < f32::EPSILON);
    }

    #[test]
    fn kendall_tau_two_elements_same_order() {
        let tau = kendall_tau(&[1, 2], &[1, 2]);
        assert!((tau - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn kendall_tau_two_elements_reversed() {
        let tau = kendall_tau(&[1, 2], &[2, 1]);
        assert!((tau - (-1.0)).abs() < f32::EPSILON);
    }

    // ---- SearchMode ----

    #[test]
    fn search_mode_debug() {
        assert_eq!(format!("{:?}", SearchMode::Lexical), "Lexical");
        assert_eq!(format!("{:?}", SearchMode::Semantic), "Semantic");
        assert_eq!(format!("{:?}", SearchMode::Hybrid), "Hybrid");
    }

    #[test]
    fn search_mode_copy_clone() {
        let m = SearchMode::Hybrid;
        let m2 = m; // Copy
        let m3 = m;
        assert_eq!(m, m2);
        assert_eq!(m, m3);
    }

    #[test]
    fn fusion_backend_copy_clone_debug() {
        let backend = FusionBackend::FrankenSearchRrf;
        let copied = backend;
        let cloned = backend;
        assert_eq!(backend, copied);
        assert_eq!(backend, cloned);
        assert_eq!(format!("{backend:?}"), "FrankenSearchRrf");
    }

    // ---- FusedResult ----

    #[test]
    fn fused_result_debug() {
        let r = FusedResult {
            id: 42,
            score: 0.5,
            lexical_rank: Some(3),
            semantic_rank: None,
        };
        let debug = format!("{r:?}");
        assert!(debug.contains("FusedResult"));
        assert!(debug.contains("42"));
    }

    #[test]
    fn fused_result_clone() {
        let r = FusedResult {
            id: 1,
            score: 0.99,
            lexical_rank: Some(0),
            semantic_rank: Some(1),
        };
        let cloned = r.clone();
        assert_eq!(cloned.id, 1);
        assert!((cloned.score - 0.99).abs() < f32::EPSILON);
        assert_eq!(cloned.lexical_rank, Some(0));
        assert_eq!(cloned.semantic_rank, Some(1));
    }

    // ---- TwoTierMetrics ----

    #[test]
    fn two_tier_metrics_default() {
        let m = TwoTierMetrics::default();
        assert_eq!(m.tier1_count, 0);
        assert_eq!(m.tier2_count, 0);
        assert_eq!(m.overlap_count, 0);
        assert!(m.rank_correlation.abs() < f32::EPSILON);
    }

    #[test]
    fn two_tier_metrics_debug_clone() {
        let m = TwoTierMetrics {
            tier1_count: 5,
            tier2_count: 3,
            overlap_count: 2,
            rank_correlation: 0.85,
        };
        let debug = format!("{m:?}");
        assert!(debug.contains("TwoTierMetrics"));
        let cloned = m.clone();
        assert_eq!(cloned.tier1_count, 5);
        assert_eq!(cloned.overlap_count, 2);
    }

    // ---- HybridSearchService ----

    #[test]
    fn hybrid_service_default_trait() {
        let svc = HybridSearchService::default();
        assert_eq!(svc.rrf_k(), 60);
        assert_eq!(svc.mode(), SearchMode::Hybrid);
    }

    #[test]
    fn hybrid_service_with_rrf_weights_negative_clamped() {
        let svc = HybridSearchService::new().with_rrf_weights(-1.0, -2.0);
        assert!(svc.lexical_weight().abs() < f32::EPSILON);
        assert!(svc.semantic_weight().abs() < f32::EPSILON);
    }

    #[test]
    fn hybrid_service_can_set_fusion_backend() {
        let svc = HybridSearchService::new().with_fusion_backend(FusionBackend::FrankenSearchRrf);
        assert_eq!(svc.fusion_backend(), FusionBackend::FrankenSearchRrf);
    }

    #[test]
    fn fusion_backend_parse_normalizes_to_frankensearch() {
        assert_eq!(
            FusionBackend::parse("legacy"),
            FusionBackend::FrankenSearchRrf
        );
        assert_eq!(FusionBackend::parse(""), FusionBackend::FrankenSearchRrf);
        assert_eq!(
            FusionBackend::parse("unknown"),
            FusionBackend::FrankenSearchRrf
        );
        assert_eq!(
            FusionBackend::parse("frankensearch"),
            FusionBackend::FrankenSearchRrf
        );
        assert_eq!(
            FusionBackend::parse("frankensearch_rrf"),
            FusionBackend::FrankenSearchRrf
        );
        assert_eq!(
            FusionBackend::parse("frankensearch-rrf"),
            FusionBackend::FrankenSearchRrf
        );
    }

    #[test]
    fn fusion_backend_as_str_roundtrip() {
        let backend = FusionBackend::FrankenSearchRrf;
        assert_eq!(FusionBackend::parse(backend.as_str()), backend);
    }

    #[test]
    fn hybrid_fuse_respects_top_k() {
        let lexical: Vec<(u64, f32)> = (0..10).map(|i| (i, 10.0 - i as f32)).collect();
        let semantic: Vec<(u64, f32)> = (10..20)
            .map(|i| (i, 1.0 - (i - 10) as f32 / 10.0))
            .collect();
        let svc = HybridSearchService::new();
        let results = svc.fuse(&lexical, &semantic, 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn frankensearch_rrf_weights_affect_ranking() {
        // B2: Weights now influence the fused ranking. A heavy lexical weight
        // should promote lexical-only items over semantic-only items.
        let lexical = vec![(10, 1.0), (20, 0.8), (30, 0.7), (40, 0.6)];
        let semantic = vec![(50, 0.9), (60, 0.85), (70, 0.8)];

        // Heavily lexical-biased
        let lex_biased = HybridSearchService::new()
            .with_fusion_backend(FusionBackend::FrankenSearchRrf)
            .with_mode(SearchMode::Hybrid)
            .with_rrf_weights(10.0, 0.1)
            .fuse(&lexical, &semantic, 7);

        // Heavily semantic-biased
        let sem_biased = HybridSearchService::new()
            .with_fusion_backend(FusionBackend::FrankenSearchRrf)
            .with_mode(SearchMode::Hybrid)
            .with_rrf_weights(0.1, 10.0)
            .fuse(&lexical, &semantic, 7);

        // With lexical bias, top result should be a lexical item
        assert!(
            [10, 20, 30, 40].contains(&lex_biased[0].id),
            "lexical bias should promote lexical items, got id={}",
            lex_biased[0].id
        );

        // With semantic bias, top result should be a semantic item
        assert!(
            [50, 60, 70].contains(&sem_biased[0].id),
            "semantic bias should promote semantic items, got id={}",
            sem_biased[0].id
        );
    }

    #[test]
    fn frankensearch_rrf_unit_weights_match_local_rrf() {
        // With unit weights, frankensearch path should produce the same ranking
        // as the local rrf_fuse (both use standard RRF formula).
        let lexical = vec![(10, 1.0), (20, 0.8), (30, 0.7)];
        let semantic = vec![(20, 0.9), (10, 0.85), (40, 0.8)];

        let fs_results = HybridSearchService::new()
            .with_fusion_backend(FusionBackend::FrankenSearchRrf)
            .with_mode(SearchMode::Hybrid)
            .with_rrf_weights(1.0, 1.0)
            .fuse(&lexical, &semantic, 10);

        let local_results = rrf_fuse(&lexical, &semantic, 60);

        let fs_ids: Vec<u64> = fs_results.iter().map(|r| r.id).collect();
        let local_ids: Vec<u64> = local_results.iter().map(|r| r.id).collect();
        assert_eq!(fs_ids, local_ids, "unit-weight frankensearch should match local RRF ranking");

        // Scores should be very close (both compute weight/(k+rank+1) with weight=1.0)
        for (fs, local) in fs_results.iter().zip(local_results.iter()) {
            assert!(
                (fs.score - local.score).abs() < 1e-5,
                "score mismatch for id={}: fs={} local={}",
                fs.id, fs.score, local.score
            );
        }
    }
}
