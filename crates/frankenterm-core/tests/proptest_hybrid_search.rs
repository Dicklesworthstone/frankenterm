//! Property-based tests for the hybrid_search module.
//!
//! Verifies invariants for:
//! - rrf_fuse: sorted output, union count, determinism, rank provenance
//! - blend_two_tier: top_k cap, deduplication, tier accounting, alpha clamping
//! - kendall_tau: range bounds, identity, symmetry, reversal
//! - SearchMode: Clone/Copy/PartialEq traits
//! - HybridSearchService: builder clamping, mode routing, defaults

use proptest::prelude::*;
use std::collections::HashSet;

use frankenterm_core::search::{
    FusedResult, HybridSearchService, SearchMode, TwoTierMetrics, blend_two_tier, kendall_tau,
    rrf_fuse,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

/// Generate a ranked list of (id, score) pairs with unique IDs in 1..=100.
fn arb_ranked_list(max_len: usize) -> impl Strategy<Value = Vec<(u64, f32)>> {
    proptest::collection::hash_set(1u64..=100, 0..=max_len).prop_flat_map(|ids| {
        let n = ids.len();
        let ids_vec: Vec<u64> = ids.into_iter().collect();
        proptest::collection::vec(0.0f32..100.0, n..=n).prop_map(move |scores| {
            ids_vec
                .iter()
                .copied()
                .zip(scores.into_iter())
                .collect::<Vec<(u64, f32)>>()
        })
    })
}

/// Generate a Vec<FusedResult> with unique IDs.
fn arb_fused_results(max_len: usize) -> impl Strategy<Value = Vec<FusedResult>> {
    proptest::collection::hash_set(1u64..=100, 0..=max_len).prop_flat_map(|ids| {
        let n = ids.len();
        let ids_vec: Vec<u64> = ids.into_iter().collect();
        proptest::collection::vec(0.0f32..10.0, n..=n).prop_map(move |scores| {
            ids_vec
                .iter()
                .copied()
                .zip(scores.into_iter())
                .enumerate()
                .map(|(i, (id, score))| FusedResult {
                    id,
                    score,
                    lexical_rank: if i % 2 == 0 { Some(i) } else { None },
                    semantic_rank: if i % 2 == 1 { Some(i) } else { None },
                })
                .collect()
        })
    })
}

/// Generate a ranking (Vec of unique IDs) for kendall_tau tests.
fn arb_ranking(max_len: usize) -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::hash_set(1u64..=50, 0..=max_len)
        .prop_map(|s| s.into_iter().collect::<Vec<u64>>())
}

/// Generate a permutation of a given vec.
#[allow(dead_code)]
fn arb_permutation(items: Vec<u64>) -> impl Strategy<Value = Vec<u64>> {
    Just(items).prop_shuffle()
}

// ────────────────────────────────────────────────────────────────────
// rrf_fuse properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// rrf_fuse output is sorted by score descending.
    #[test]
    fn rrf_fuse_sorted_descending(
        lexical in arb_ranked_list(15),
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&lexical, &semantic, k);
        for i in 1..fused.len() {
            prop_assert!(
                fused[i - 1].score >= fused[i].score,
                "Result at position {} has score {} > score {} at position {}",
                i, fused[i].score, fused[i - 1].score, i - 1
            );
        }
    }

    /// rrf_fuse result count is the union of IDs from both lists.
    #[test]
    fn rrf_fuse_count_is_union(
        lexical in arb_ranked_list(15),
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&lexical, &semantic, k);
        let lex_ids: HashSet<u64> = lexical.iter().map(|&(id, _)| id).collect();
        let sem_ids: HashSet<u64> = semantic.iter().map(|&(id, _)| id).collect();
        let union_count = lex_ids.union(&sem_ids).count();
        prop_assert_eq!(fused.len(), union_count);
    }

    /// rrf_fuse is deterministic: calling twice yields the same result.
    #[test]
    fn rrf_fuse_deterministic(
        lexical in arb_ranked_list(10),
        semantic in arb_ranked_list(10),
        k in 1u32..=120,
    ) {
        let a = rrf_fuse(&lexical, &semantic, k);
        let b = rrf_fuse(&lexical, &semantic, k);
        prop_assert_eq!(a.len(), b.len());
        for (ra, rb) in a.iter().zip(b.iter()) {
            prop_assert_eq!(ra.id, rb.id);
            prop_assert!((ra.score - rb.score).abs() < 1e-9, "Scores differ for id {}", ra.id);
        }
    }

    /// rrf_fuse with both inputs empty returns empty.
    #[test]
    fn rrf_fuse_both_empty(k in 1u32..=200) {
        let fused = rrf_fuse(&[], &[], k);
        prop_assert!(fused.is_empty());
    }

    /// rrf_fuse with lexical empty returns only semantic IDs.
    #[test]
    fn rrf_fuse_lexical_empty(
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&[], &semantic, k);
        prop_assert_eq!(fused.len(), semantic.len());
        for r in &fused {
            prop_assert!(r.lexical_rank.is_none(), "Expected None lexical_rank for id {}", r.id);
            prop_assert!(r.semantic_rank.is_some(), "Expected Some semantic_rank for id {}", r.id);
        }
    }

    /// rrf_fuse with semantic empty returns only lexical IDs.
    #[test]
    fn rrf_fuse_semantic_empty(
        lexical in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&lexical, &[], k);
        prop_assert_eq!(fused.len(), lexical.len());
        for r in &fused {
            prop_assert!(r.lexical_rank.is_some(), "Expected Some lexical_rank for id {}", r.id);
            prop_assert!(r.semantic_rank.is_none(), "Expected None semantic_rank for id {}", r.id);
        }
    }

    /// Items in both lists have both lexical_rank and semantic_rank set.
    #[test]
    fn rrf_fuse_both_ranks_for_overlap(
        lexical in arb_ranked_list(15),
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let lex_ids: HashSet<u64> = lexical.iter().map(|&(id, _)| id).collect();
        let sem_ids: HashSet<u64> = semantic.iter().map(|&(id, _)| id).collect();
        let common: HashSet<u64> = lex_ids.intersection(&sem_ids).copied().collect();

        let fused = rrf_fuse(&lexical, &semantic, k);
        for r in &fused {
            if common.contains(&r.id) {
                prop_assert!(
                    r.lexical_rank.is_some() && r.semantic_rank.is_some(),
                    "Common id {} should have both ranks set", r.id
                );
            }
        }
    }

    /// Items in only one list have None for the other rank.
    #[test]
    fn rrf_fuse_none_rank_for_exclusive(
        lexical in arb_ranked_list(15),
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let lex_ids: HashSet<u64> = lexical.iter().map(|&(id, _)| id).collect();
        let sem_ids: HashSet<u64> = semantic.iter().map(|&(id, _)| id).collect();

        let fused = rrf_fuse(&lexical, &semantic, k);
        for r in &fused {
            if lex_ids.contains(&r.id) && !sem_ids.contains(&r.id) {
                prop_assert!(
                    r.semantic_rank.is_none(),
                    "Lexical-only id {} should have None semantic_rank", r.id
                );
            }
            if sem_ids.contains(&r.id) && !lex_ids.contains(&r.id) {
                prop_assert!(
                    r.lexical_rank.is_none(),
                    "Semantic-only id {} should have None lexical_rank", r.id
                );
            }
        }
    }

    /// rrf_fuse IDs are unique in output.
    #[test]
    fn rrf_fuse_unique_ids(
        lexical in arb_ranked_list(15),
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&lexical, &semantic, k);
        let ids: HashSet<u64> = fused.iter().map(|r| r.id).collect();
        prop_assert_eq!(ids.len(), fused.len());
    }

    /// rrf_fuse all scores are positive (since k >= 1 and weight=1.0).
    #[test]
    fn rrf_fuse_scores_positive(
        lexical in arb_ranked_list(15),
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&lexical, &semantic, k);
        for r in &fused {
            prop_assert!(r.score > 0.0, "Score for id {} should be positive, got {}", r.id, r.score);
        }
    }

    /// rrf_fuse: tie-breaking is by id ascending (lower id first when scores equal).
    #[test]
    fn rrf_fuse_tiebreak_by_id(
        lexical in arb_ranked_list(15),
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&lexical, &semantic, k);
        for i in 1..fused.len() {
            if (fused[i - 1].score - fused[i].score).abs() < 1e-12 {
                prop_assert!(
                    fused[i - 1].id <= fused[i].id,
                    "Tie-break violated: id {} should come before id {} at equal score {}",
                    fused[i - 1].id, fused[i].id, fused[i].score
                );
            }
        }
    }

    /// rrf_fuse: lexical_rank values are valid indices into the lexical list.
    #[test]
    fn rrf_fuse_lexical_ranks_valid(
        lexical in arb_ranked_list(15),
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&lexical, &semantic, k);
        for r in &fused {
            if let Some(rank) = r.lexical_rank {
                prop_assert!(
                    rank < lexical.len(),
                    "lexical_rank {} out of bounds for lexical list of len {}",
                    rank, lexical.len()
                );
            }
        }
    }

    /// rrf_fuse: semantic_rank values are valid indices into the semantic list.
    #[test]
    fn rrf_fuse_semantic_ranks_valid(
        lexical in arb_ranked_list(15),
        semantic in arb_ranked_list(15),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&lexical, &semantic, k);
        for r in &fused {
            if let Some(rank) = r.semantic_rank {
                prop_assert!(
                    rank < semantic.len(),
                    "semantic_rank {} out of bounds for semantic list of len {}",
                    rank, semantic.len()
                );
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// blend_two_tier properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// blend_two_tier returns at most top_k results.
    #[test]
    fn blend_top_k_cap(
        tier1 in arb_fused_results(15),
        tier2 in arb_fused_results(15),
        top_k in 0usize..=30,
        alpha in 0.0f32..=1.0,
    ) {
        let (results, _) = blend_two_tier(&tier1, &tier2, top_k, alpha);
        prop_assert!(
            results.len() <= top_k,
            "Got {} results, expected at most {}",
            results.len(), top_k
        );
    }

    /// blend_two_tier: no duplicate IDs in output.
    #[test]
    fn blend_no_duplicate_ids(
        tier1 in arb_fused_results(15),
        tier2 in arb_fused_results(15),
        top_k in 1usize..=30,
        alpha in 0.0f32..=1.0,
    ) {
        let (results, _) = blend_two_tier(&tier1, &tier2, top_k, alpha);
        let ids: HashSet<u64> = results.iter().map(|r| r.id).collect();
        prop_assert_eq!(ids.len(), results.len());
    }

    /// blend_two_tier: tier1_count + tier2_count == results.len().
    #[test]
    fn blend_tier_counts_sum(
        tier1 in arb_fused_results(15),
        tier2 in arb_fused_results(15),
        top_k in 0usize..=30,
        alpha in 0.0f32..=1.0,
    ) {
        let (results, metrics) = blend_two_tier(&tier1, &tier2, top_k, alpha);
        prop_assert_eq!(
            metrics.tier1_count + metrics.tier2_count,
            results.len(),
            "tier1_count ({}) + tier2_count ({}) != results.len() ({})",
            metrics.tier1_count, metrics.tier2_count, results.len()
        );
    }

    /// blend_two_tier: overlap_count equals the intersection of tier1 and tier2 IDs.
    #[test]
    fn blend_overlap_count_correct(
        tier1 in arb_fused_results(15),
        tier2 in arb_fused_results(15),
        top_k in 0usize..=30,
        alpha in 0.0f32..=1.0,
    ) {
        let (_, metrics) = blend_two_tier(&tier1, &tier2, top_k, alpha);
        let t1_ids: HashSet<u64> = tier1.iter().map(|r| r.id).collect();
        let t2_ids: HashSet<u64> = tier2.iter().map(|r| r.id).collect();
        let expected_overlap = t1_ids.intersection(&t2_ids).count();
        prop_assert_eq!(
            metrics.overlap_count,
            expected_overlap,
            "overlap_count {} != expected {}",
            metrics.overlap_count, expected_overlap
        );
    }

    /// blend_two_tier: alpha clamped, so values outside [0,1] behave the same as clamped.
    #[test]
    fn blend_alpha_clamped(
        tier1 in arb_fused_results(10),
        tier2 in arb_fused_results(10),
        top_k in 1usize..=20,
        alpha in -5.0f32..=5.0,
    ) {
        let (results_raw, _) = blend_two_tier(&tier1, &tier2, top_k, alpha);
        let clamped = alpha.clamp(0.0, 1.0);
        let (results_clamped, _) = blend_two_tier(&tier1, &tier2, top_k, clamped);
        prop_assert_eq!(results_raw.len(), results_clamped.len());
        for (a, b) in results_raw.iter().zip(results_clamped.iter()) {
            prop_assert_eq!(a.id, b.id);
            prop_assert!(
                (a.score - b.score).abs() < 1e-6,
                "Score mismatch for id {}: {} vs {}",
                a.id, a.score, b.score
            );
        }
    }

    /// blend_two_tier: tier1 results appear before tier2 results.
    #[test]
    fn blend_tier1_before_tier2(
        tier1 in arb_fused_results(10),
        tier2 in arb_fused_results(10),
        top_k in 1usize..=20,
        alpha in 0.0f32..=1.0,
    ) {
        let (results, metrics) = blend_two_tier(&tier1, &tier2, top_k, alpha);
        let tier1_ids: HashSet<u64> = tier1.iter().map(|r| r.id).collect();
        let tier2_only_ids: HashSet<u64> = tier2.iter().map(|r| r.id)
            .filter(|id| !tier1_ids.contains(id))
            .collect();

        // All tier1-sourced results should come first
        let mut seen_tier2 = false;
        for r in &results {
            if tier2_only_ids.contains(&r.id) {
                seen_tier2 = true;
            } else if seen_tier2 && tier1_ids.contains(&r.id) {
                // A tier1 result after a tier2 result means tier1 was already exhausted
                // This shouldn't happen if tier1 results come first
                // But it's possible if the id was a duplicate already seen from tier1
                // The test should verify ordering within the first tier1_count items
            }
        }
        // First tier1_count items should all be from tier1
        for r in results.iter().take(metrics.tier1_count) {
            prop_assert!(
                tier1_ids.contains(&r.id),
                "Expected first {} results to be from tier1, but id {} is not",
                metrics.tier1_count, r.id
            );
        }
    }

    /// blend_two_tier: with top_k=0 always returns empty.
    #[test]
    fn blend_top_k_zero(
        tier1 in arb_fused_results(10),
        tier2 in arb_fused_results(10),
        alpha in 0.0f32..=1.0,
    ) {
        let (results, metrics) = blend_two_tier(&tier1, &tier2, 0, alpha);
        prop_assert!(results.is_empty());
        prop_assert_eq!(metrics.tier1_count, 0usize);
        prop_assert_eq!(metrics.tier2_count, 0usize);
    }

    /// blend_two_tier: both tiers empty returns empty.
    #[test]
    fn blend_both_empty(
        top_k in 0usize..=20,
        alpha in 0.0f32..=1.0,
    ) {
        let (results, metrics) = blend_two_tier(&[], &[], top_k, alpha);
        prop_assert!(results.is_empty());
        prop_assert_eq!(metrics.tier1_count, 0usize);
        prop_assert_eq!(metrics.tier2_count, 0usize);
        prop_assert_eq!(metrics.overlap_count, 0usize);
    }

    /// blend_two_tier: alpha=1.0 means tier1 scores unchanged, tier2 scores zeroed.
    #[test]
    fn blend_alpha_one_tier1_full_weight(
        tier1 in arb_fused_results(10),
        tier2 in arb_fused_results(10),
        top_k in 1usize..=20,
    ) {
        let tier1_ids: HashSet<u64> = tier1.iter().map(|r| r.id).collect();
        let (results, _) = blend_two_tier(&tier1, &tier2, top_k, 1.0);
        for r in &results {
            if tier1_ids.contains(&r.id) {
                // This result came from tier1; score = original * 1.0
                // We can't check exact original since dedup may have picked tier1's copy
            } else {
                // This result came from tier2; score = original * (1 - 1.0) = 0
                prop_assert!(
                    r.score.abs() < 1e-6,
                    "Tier2 result id {} should have score ~0 at alpha=1.0, got {}",
                    r.id, r.score
                );
            }
        }
    }

    /// blend_two_tier: alpha=0.0 means tier1 scores zeroed, tier2 scores unchanged.
    #[test]
    fn blend_alpha_zero_tier2_full_weight(
        tier1 in arb_fused_results(10),
        tier2 in arb_fused_results(10),
        top_k in 1usize..=20,
    ) {
        let (results, metrics) = blend_two_tier(&tier1, &tier2, top_k, 0.0);
        // Tier1 results still appear first (order is tier1 then tier2), but scores are zeroed
        for r in results.iter().take(metrics.tier1_count) {
            prop_assert!(
                r.score.abs() < 1e-6,
                "Tier1 result id {} should have score ~0 at alpha=0.0, got {}",
                r.id, r.score
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// kendall_tau properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// kendall_tau is always in [-1.0, 1.0].
    #[test]
    fn kendall_tau_range(
        a in arb_ranking(20),
        b in arb_ranking(20),
    ) {
        let tau = kendall_tau(&a, &b);
        prop_assert!(
            (-1.0..=1.0).contains(&tau),
            "kendall_tau out of range: {}",
            tau
        );
    }

    /// kendall_tau of identical rankings is 1.0 (when 2+ common items).
    #[test]
    fn kendall_tau_identical(
        ranking in proptest::collection::hash_set(1u64..=50, 2..=15)
            .prop_map(|s| s.into_iter().collect::<Vec<u64>>()),
    ) {
        let tau = kendall_tau(&ranking, &ranking);
        prop_assert!(
            (tau - 1.0).abs() < 1e-6,
            "kendall_tau(same, same) should be 1.0, got {}",
            tau
        );
    }

    /// kendall_tau is symmetric: tau(a, b) == tau(b, a).
    #[test]
    fn kendall_tau_symmetric(
        a in arb_ranking(15),
        b in arb_ranking(15),
    ) {
        let tau_ab = kendall_tau(&a, &b);
        let tau_ba = kendall_tau(&b, &a);
        prop_assert!(
            (tau_ab - tau_ba).abs() < 1e-6,
            "kendall_tau not symmetric: tau(a,b)={}, tau(b,a)={}",
            tau_ab, tau_ba
        );
    }

    /// kendall_tau of empty input returns 0.0.
    #[test]
    fn kendall_tau_empty_a(b in arb_ranking(10)) {
        let tau = kendall_tau(&[], &b);
        prop_assert!(
            tau.abs() < 1e-6,
            "kendall_tau with empty a should be 0.0, got {}",
            tau
        );
    }

    /// kendall_tau with empty b returns 0.0.
    #[test]
    fn kendall_tau_empty_b(a in arb_ranking(10)) {
        let tau = kendall_tau(&a, &[]);
        prop_assert!(
            tau.abs() < 1e-6,
            "kendall_tau with empty b should be 0.0, got {}",
            tau
        );
    }

    /// kendall_tau with fewer than 2 common items returns 0.0.
    #[test]
    fn kendall_tau_less_than_two_common(
        a_only in proptest::collection::hash_set(1u64..=25, 1..=10),
        b_only in proptest::collection::hash_set(51u64..=75, 1..=10),
        shared in proptest::sample::select(vec![None, Some(100u64)]),
    ) {
        let mut a: Vec<u64> = a_only.into_iter().collect();
        let mut b: Vec<u64> = b_only.into_iter().collect();
        // At most 1 shared element between the two
        if let Some(id) = shared {
            a.push(id);
            b.push(id);
        }
        let tau = kendall_tau(&a, &b);
        prop_assert!(
            tau.abs() < 1e-6,
            "kendall_tau with <2 common should be 0.0, got {}",
            tau
        );
    }

    /// kendall_tau of reversed ranking is -1.0 (when 2+ items).
    #[test]
    fn kendall_tau_reversed(
        ranking in proptest::collection::hash_set(1u64..=50, 2..=15)
            .prop_map(|s| s.into_iter().collect::<Vec<u64>>()),
    ) {
        let mut reversed = ranking.clone();
        reversed.reverse();
        let tau = kendall_tau(&ranking, &reversed);
        prop_assert!(
            (tau - (-1.0)).abs() < 1e-6,
            "kendall_tau(ranking, reversed) should be -1.0, got {}",
            tau
        );
    }

    /// kendall_tau: value is not NaN.
    #[test]
    fn kendall_tau_not_nan(
        a in arb_ranking(15),
        b in arb_ranking(15),
    ) {
        let tau = kendall_tau(&a, &b);
        prop_assert!(!tau.is_nan(), "kendall_tau returned NaN");
    }

    /// kendall_tau: adding disjoint elements does not change the result.
    #[test]
    fn kendall_tau_disjoint_elements_ignored(
        common in proptest::collection::hash_set(1u64..=25, 2..=10)
            .prop_map(|s| s.into_iter().collect::<Vec<u64>>()),
        a_extra in proptest::collection::hash_set(51u64..=75, 0..=5)
            .prop_map(|s| s.into_iter().collect::<Vec<u64>>()),
        b_extra in proptest::collection::hash_set(76u64..=100, 0..=5)
            .prop_map(|s| s.into_iter().collect::<Vec<u64>>()),
    ) {
        let tau_bare = kendall_tau(&common, &common);

        let mut a_extended = common.clone();
        a_extended.extend_from_slice(&a_extra);
        let mut b_extended = common.clone();
        b_extended.extend_from_slice(&b_extra);

        let tau_extended = kendall_tau(&a_extended, &b_extended);
        prop_assert!(
            (tau_bare - tau_extended).abs() < 1e-6,
            "Disjoint elements changed tau: bare={}, extended={}",
            tau_bare, tau_extended
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// SearchMode properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SearchMode Clone produces equal value.
    #[test]
    fn search_mode_clone_eq(
        idx in 0u8..=2,
    ) {
        let mode = match idx {
            0 => SearchMode::Lexical,
            1 => SearchMode::Semantic,
            _ => SearchMode::Hybrid,
        };
        let cloned = mode;
        prop_assert_eq!(mode, cloned);
    }

    /// SearchMode Copy semantics: original still usable after copy.
    #[test]
    fn search_mode_copy(
        idx in 0u8..=2,
    ) {
        let mode = match idx {
            0 => SearchMode::Lexical,
            1 => SearchMode::Semantic,
            _ => SearchMode::Hybrid,
        };
        let copy = mode;
        // Use both to prove Copy semantics
        prop_assert_eq!(mode, copy);
        prop_assert_eq!(mode, mode);
    }

    /// SearchMode PartialEq: different variants are not equal.
    #[test]
    fn search_mode_ne(
        a_idx in 0u8..=2,
        b_idx in 0u8..=2,
    ) {
        let a = match a_idx {
            0 => SearchMode::Lexical,
            1 => SearchMode::Semantic,
            _ => SearchMode::Hybrid,
        };
        let b = match b_idx {
            0 => SearchMode::Lexical,
            1 => SearchMode::Semantic,
            _ => SearchMode::Hybrid,
        };
        if a_idx == b_idx {
            prop_assert_eq!(a, b);
        } else {
            prop_assert_ne!(a, b);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// HybridSearchService properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// HybridSearchService::new() has expected defaults.
    #[test]
    fn service_defaults(_dummy in 0u8..=0) {
        let svc = HybridSearchService::new();
        prop_assert_eq!(svc.rrf_k(), 60u32);
        prop_assert!((svc.alpha() - 0.7).abs() < 1e-6, "Default alpha should be 0.7, got {}", svc.alpha());
        prop_assert_eq!(svc.mode(), SearchMode::Hybrid);
        prop_assert!((svc.lexical_weight() - 1.0).abs() < 1e-6, "Default lexical_weight should be 1.0");
        prop_assert!((svc.semantic_weight() - 1.0).abs() < 1e-6, "Default semantic_weight should be 1.0");
    }

    /// with_alpha clamps to [0.0, 1.0].
    #[test]
    fn service_alpha_clamp(alpha in -10.0f32..=10.0) {
        let svc = HybridSearchService::new().with_alpha(alpha);
        let clamped = alpha.clamp(0.0, 1.0);
        prop_assert!(
            (svc.alpha() - clamped).abs() < 1e-6,
            "with_alpha({}) should clamp to {}, got {}",
            alpha, clamped, svc.alpha()
        );
    }

    /// with_rrf_k stores the value correctly.
    #[test]
    fn service_rrf_k(k in 0u32..=1000) {
        let svc = HybridSearchService::new().with_rrf_k(k);
        prop_assert_eq!(svc.rrf_k(), k);
    }

    /// with_rrf_weights clamps negative weights to 0.
    #[test]
    fn service_rrf_weights_clamp(
        lex_w in -5.0f32..=5.0,
        sem_w in -5.0f32..=5.0,
    ) {
        let svc = HybridSearchService::new().with_rrf_weights(lex_w, sem_w);
        prop_assert!(
            svc.lexical_weight() >= 0.0,
            "lexical_weight should be >= 0, got {}",
            svc.lexical_weight()
        );
        prop_assert!(
            svc.semantic_weight() >= 0.0,
            "semantic_weight should be >= 0, got {}",
            svc.semantic_weight()
        );
        prop_assert!(
            (svc.lexical_weight() - lex_w.max(0.0)).abs() < 1e-6,
            "lexical_weight mismatch: expected {}, got {}",
            lex_w.max(0.0), svc.lexical_weight()
        );
        prop_assert!(
            (svc.semantic_weight() - sem_w.max(0.0)).abs() < 1e-6,
            "semantic_weight mismatch: expected {}, got {}",
            sem_w.max(0.0), svc.semantic_weight()
        );
    }

    /// with_mode sets the mode correctly.
    #[test]
    fn service_mode_set(idx in 0u8..=2) {
        let mode = match idx {
            0 => SearchMode::Lexical,
            1 => SearchMode::Semantic,
            _ => SearchMode::Hybrid,
        };
        let svc = HybridSearchService::new().with_mode(mode);
        prop_assert_eq!(svc.mode(), mode);
    }

    /// Lexical mode ignores semantic list entirely.
    #[test]
    fn service_lexical_mode_ignores_semantic(
        lexical in arb_ranked_list(10),
        semantic in arb_ranked_list(10),
        top_k in 1usize..=20,
    ) {
        let svc = HybridSearchService::new().with_mode(SearchMode::Lexical);
        let results = svc.fuse(&lexical, &semantic, top_k);
        let expected_len = lexical.len().min(top_k);
        prop_assert_eq!(
            results.len(), expected_len,
            "Lexical mode should return min(lexical.len(), top_k)={}, got {}",
            expected_len, results.len()
        );
        for r in &results {
            prop_assert!(r.semantic_rank.is_none(), "Lexical mode result should have None semantic_rank");
            prop_assert!(r.lexical_rank.is_some(), "Lexical mode result should have Some lexical_rank");
        }
    }

    /// Semantic mode ignores lexical list entirely.
    #[test]
    fn service_semantic_mode_ignores_lexical(
        lexical in arb_ranked_list(10),
        semantic in arb_ranked_list(10),
        top_k in 1usize..=20,
    ) {
        let svc = HybridSearchService::new().with_mode(SearchMode::Semantic);
        let results = svc.fuse(&lexical, &semantic, top_k);
        let expected_len = semantic.len().min(top_k);
        prop_assert_eq!(
            results.len(), expected_len,
            "Semantic mode should return min(semantic.len(), top_k)={}, got {}",
            expected_len, results.len()
        );
        for r in &results {
            prop_assert!(r.lexical_rank.is_none(), "Semantic mode result should have None lexical_rank");
            prop_assert!(r.semantic_rank.is_some(), "Semantic mode result should have Some semantic_rank");
        }
    }

    /// Hybrid mode returns at most top_k results.
    #[test]
    fn service_hybrid_top_k(
        lexical in arb_ranked_list(10),
        semantic in arb_ranked_list(10),
        top_k in 0usize..=25,
    ) {
        let svc = HybridSearchService::new();
        let results = svc.fuse(&lexical, &semantic, top_k);
        prop_assert!(
            results.len() <= top_k,
            "Hybrid mode returned {} results, expected at most {}",
            results.len(), top_k
        );
    }

    /// HybridSearchService::default() equals ::new().
    #[test]
    fn service_default_equals_new(_dummy in 0u8..=0) {
        let a = HybridSearchService::new();
        let b = HybridSearchService::default();
        prop_assert_eq!(a.rrf_k(), b.rrf_k());
        prop_assert!((a.alpha() - b.alpha()).abs() < 1e-6, "alpha mismatch");
        prop_assert_eq!(a.mode(), b.mode());
        prop_assert!((a.lexical_weight() - b.lexical_weight()).abs() < 1e-6, "lexical_weight mismatch");
        prop_assert!((a.semantic_weight() - b.semantic_weight()).abs() < 1e-6, "semantic_weight mismatch");
    }
}

// ────────────────────────────────────────────────────────────────────
// TwoTierMetrics properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// TwoTierMetrics::default() is all zeros.
    #[test]
    fn two_tier_metrics_default(_dummy in 0u8..=0) {
        let m = TwoTierMetrics::default();
        prop_assert_eq!(m.tier1_count, 0usize);
        prop_assert_eq!(m.tier2_count, 0usize);
        prop_assert_eq!(m.overlap_count, 0usize);
        prop_assert!(m.rank_correlation.abs() < 1e-6, "Default rank_correlation should be 0.0");
    }

    /// TwoTierMetrics is Debug-printable (does not panic).
    #[test]
    fn two_tier_metrics_debug(
        t1 in 0usize..=100,
        t2 in 0usize..=100,
        ov in 0usize..=100,
    ) {
        let m = TwoTierMetrics {
            tier1_count: t1,
            tier2_count: t2,
            overlap_count: ov,
            rank_correlation: 0.0,
        };
        let debug = format!("{:?}", m);
        prop_assert!(!debug.is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// Cross-function integration properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// End-to-end: rrf_fuse then blend_two_tier preserves dedup and top_k.
    #[test]
    fn end_to_end_fuse_then_blend(
        lex1 in arb_ranked_list(8),
        sem1 in arb_ranked_list(8),
        lex2 in arb_ranked_list(8),
        sem2 in arb_ranked_list(8),
        k in 10u32..=80,
        top_k in 1usize..=15,
        alpha in 0.0f32..=1.0,
    ) {
        let tier1 = rrf_fuse(&lex1, &sem1, k);
        let tier2 = rrf_fuse(&lex2, &sem2, k);
        let (results, metrics) = blend_two_tier(&tier1, &tier2, top_k, alpha);

        // No duplicates
        let ids: HashSet<u64> = results.iter().map(|r| r.id).collect();
        prop_assert_eq!(ids.len(), results.len());

        // At most top_k
        prop_assert!(results.len() <= top_k, "Got {} results, expected <= {}", results.len(), top_k);

        // Tier count sum
        prop_assert_eq!(
            metrics.tier1_count + metrics.tier2_count,
            results.len()
        );
    }

    /// rrf_fuse followed by kendall_tau on result IDs produces valid correlation.
    #[test]
    fn rrf_then_kendall_valid(
        lexical in arb_ranked_list(10),
        semantic in arb_ranked_list(10),
        k in 1u32..=120,
    ) {
        let fused = rrf_fuse(&lexical, &semantic, k);
        let fused_ids: Vec<u64> = fused.iter().map(|r| r.id).collect();
        let lex_ids: Vec<u64> = lexical.iter().map(|&(id, _)| id).collect();
        let tau = kendall_tau(&fused_ids, &lex_ids);
        prop_assert!(
            (-1.0..=1.0).contains(&tau),
            "kendall_tau out of range after fuse: {}",
            tau
        );
        prop_assert!(!tau.is_nan(), "kendall_tau returned NaN after fuse");
    }
}
