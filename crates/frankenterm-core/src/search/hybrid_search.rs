//! Reciprocal Rank Fusion, two-tier blending, and hybrid search orchestration.

use std::collections::HashMap;

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

/// A fused search result with combined score.
#[derive(Debug, Clone)]
pub struct FusedResult {
    pub id: u64,
    pub score: f32,
    pub lexical_rank: Option<usize>,
    pub semantic_rank: Option<usize>,
}

/// Metrics from two-tier blending.
#[derive(Debug, Clone, Default)]
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
    let mut scores: HashMap<u64, (f32, Option<usize>, Option<usize>)> = HashMap::new();

    for (rank, &(id, _score)) in lexical.iter().enumerate() {
        let entry = scores.entry(id).or_insert((0.0, None, None));
        entry.0 += 1.0 / (k as f32 + rank as f32 + 1.0);
        entry.1 = Some(rank);
    }

    for (rank, &(id, _score)) in semantic.iter().enumerate() {
        let entry = scores.entry(id).or_insert((0.0, None, None));
        entry.0 += 1.0 / (k as f32 + rank as f32 + 1.0);
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
    });
    results
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

    let mut seen: HashMap<u64, ()> = HashMap::new();
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
        if seen.contains_key(&r.id) {
            continue;
        }
        seen.insert(r.id, ());
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
        if seen.contains_key(&r.id) {
            continue;
        }
        seen.insert(r.id, ());
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
}

impl HybridSearchService {
    pub fn new() -> Self {
        Self {
            rrf_k: 60,
            alpha: 0.7,
            mode: SearchMode::Hybrid,
        }
    }

    pub fn with_rrf_k(mut self, k: u32) -> Self {
        self.rrf_k = k;
        self
    }

    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.alpha = alpha.clamp(0.0, 1.0);
        self
    }

    pub fn with_mode(mut self, mode: SearchMode) -> Self {
        self.mode = mode;
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
                let fused = rrf_fuse(lexical, semantic, self.rrf_k);
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
        assert_eq!(results[0].score, 0.0); // 1.0 * 0.0
        assert_eq!(results[1].score, 0.5); // 0.5 * 1.0
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
        assert_eq!(kendall_tau(&[], &[1, 2, 3]), 0.0);
        assert_eq!(kendall_tau(&[1], &[]), 0.0);
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
        assert_eq!(tau, 0.0);
    }

    #[test]
    fn hybrid_service_defaults() {
        let svc = HybridSearchService::new();
        assert_eq!(svc.rrf_k(), 60);
        assert!((svc.alpha() - 0.7).abs() < f32::EPSILON);
        assert_eq!(svc.mode(), SearchMode::Hybrid);
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
}
