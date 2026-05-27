//! Conformance harness for Reciprocal Rank Fusion (RRF) hybrid ranking.
//!
//! The hybrid search stack fuses a lexical (FTS5) ranked list and a semantic
//! (vector) ranked list into a single ordering. The canonical specification is
//! Cormack, Clarke & Buettcher (2009), *Reciprocal Rank Fusion outperforms
//! Condorcet and individual Rank Learning Methods*:
//!
//! ```text
//!   RRFscore(d) = Σ_{r ∈ rankers}  1 / (k + rank_r(d))
//! ```
//!
//! FrankenTerm's implementation (`search::rrf_fuse`) uses **0-indexed** ranks,
//! so the per-lane contribution is `weight / (k + rank + 1)`, and adds per-lane
//! weights plus deterministic id-ascending tie-breaking. This harness pins that
//! contract against an *independent reference oracle* (so arithmetic, sort, or
//! dedup drift is caught by exact comparison) and against named spec properties
//! that hold regardless of how the score happens to be computed.
//!
//! These tests are deliberately oracle-based and feature-independent: they only
//! touch the public, deterministic `search::{rrf_fuse, blend_two_tier,
//! kendall_tau, HybridSearchService}` surface. They do not require the
//! `frankensearch` feature — the local fusion path is the conformance target.

use frankenterm_core::search::{
    FusedResult, HybridSearchService, SearchMode, blend_two_tier, kendall_tau, rrf_fuse,
};
use std::collections::BTreeMap;

/// f32 tolerance for score comparisons. The reference oracle uses the same f32
/// arithmetic and summation order as the implementation, so agreement is exact
/// in practice; the epsilon only guards against incidental reassociation.
const EPS: f32 = 1e-6;

// =============================================================================
// Independent reference oracle
// =============================================================================

/// Reference RRF, written independently of the production implementation.
///
/// Spec:
/// - Each lane is deduped keeping the **first** occurrence of an id; that
///   occurrence's 0-based position is its rank.
/// - A lane with weight `<= 0.0` contributes no score but still stamps its rank.
/// - `score(id) = lex_w/(k+lex_rank+1) [if present] + sem_w/(k+sem_rank+1) [if present]`.
/// - Results are sorted by score descending, ties broken by id ascending.
fn reference_rrf(
    lexical: &[(u64, f32)],
    semantic: &[(u64, f32)],
    k: u32,
    lex_w: f32,
    sem_w: f32,
) -> Vec<FusedResult> {
    fn dedupe(items: &[(u64, f32)]) -> Vec<u64> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for &(id, _) in items {
            if seen.insert(id) {
                out.push(id);
            }
        }
        out
    }

    let lex = dedupe(lexical);
    let sem = dedupe(semantic);
    let k_f = k as f32;

    // BTreeMap keeps a stable, id-sorted iteration so the reference is itself
    // deterministic before the score sort.
    let mut acc: BTreeMap<u64, (f32, Option<usize>, Option<usize>)> = BTreeMap::new();

    for (rank, id) in lex.iter().enumerate() {
        let e = acc.entry(*id).or_insert((0.0, None, None));
        e.1 = Some(rank);
        if lex_w > 0.0 {
            e.0 += lex_w / (k_f + rank as f32 + 1.0);
        }
    }
    for (rank, id) in sem.iter().enumerate() {
        let e = acc.entry(*id).or_insert((0.0, None, None));
        e.2 = Some(rank);
        if sem_w > 0.0 {
            e.0 += sem_w / (k_f + rank as f32 + 1.0);
        }
    }

    let mut out: Vec<FusedResult> = acc
        .into_iter()
        .map(|(id, (score, lr, sr))| FusedResult {
            id,
            score,
            lexical_rank: lr,
            semantic_rank: sr,
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

fn assert_fused_eq(label: &str, got: &[FusedResult], expected: &[FusedResult]) {
    assert_eq!(
        got.len(),
        expected.len(),
        "{label}: result count mismatch ({} vs {})",
        got.len(),
        expected.len()
    );
    for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
        assert_eq!(g.id, e.id, "{label}: id mismatch at position {i}");
        assert!(
            (g.score - e.score).abs() <= EPS,
            "{label}: score mismatch at position {i} for id {}: got {} expected {}",
            g.id,
            g.score,
            e.score
        );
        assert_eq!(
            g.lexical_rank, e.lexical_rank,
            "{label}: lexical_rank mismatch at position {i} for id {}",
            g.id
        );
        assert_eq!(
            g.semantic_rank, e.semantic_rank,
            "{label}: semantic_rank mismatch at position {i} for id {}",
            g.id
        );
    }
}

/// A deterministic battery of fusion scenarios spanning the interesting axes:
/// full/partial/no overlap, duplicate ids within a lane, single-lane, and empty.
fn corpus() -> Vec<(&'static str, Vec<(u64, f32)>, Vec<(u64, f32)>)> {
    vec![
        ("empty", vec![], vec![]),
        ("lexical_only", vec![(1, 9.0), (2, 8.0), (3, 7.0)], vec![]),
        ("semantic_only", vec![], vec![(5, 0.9), (6, 0.8)]),
        (
            "disjoint",
            vec![(1, 9.0), (2, 8.0), (3, 7.0)],
            vec![(4, 0.9), (5, 0.8), (6, 0.7)],
        ),
        (
            "full_overlap_same_order",
            vec![(1, 9.0), (2, 8.0), (3, 7.0)],
            vec![(1, 0.9), (2, 0.8), (3, 0.7)],
        ),
        (
            "full_overlap_reversed",
            vec![(1, 9.0), (2, 8.0), (3, 7.0)],
            vec![(3, 0.9), (2, 0.8), (1, 0.7)],
        ),
        (
            "partial_overlap",
            vec![(1, 9.0), (3, 8.0), (5, 7.0)],
            vec![(3, 0.95), (2, 0.85), (7, 0.75)],
        ),
        (
            "dup_in_lexical",
            vec![(1, 9.0), (1, 5.0), (2, 8.0)],
            vec![(2, 0.9), (3, 0.8)],
        ),
        (
            "dup_in_both",
            vec![(1, 9.0), (2, 8.0), (2, 1.0)],
            vec![(2, 0.9), (2, 0.1), (1, 0.5)],
        ),
        (
            "ten_each_offset_overlap",
            (0..10u64).map(|i| (i, 10.0 - i as f32)).collect(),
            (5..15u64).map(|i| (i, 1.0 - i as f32 / 100.0)).collect(),
        ),
    ]
}

// =============================================================================
// 1. RRF score conformance vs. independent oracle
// =============================================================================

#[test]
fn rrf_matches_reference_across_corpus_and_k() {
    for k in [0u32, 1, 10, 60, 1000] {
        for (label, lex, sem) in corpus() {
            let got = rrf_fuse(&lex, &sem, k);
            let expected = reference_rrf(&lex, &sem, k, 1.0, 1.0);
            assert_fused_eq(&format!("{label} (k={k})"), &got, &expected);
        }
    }
}

#[test]
fn weighted_rrf_matches_reference_via_service() {
    // The service Hybrid path applies per-lane weights. With the local fusion
    // backend (frankensearch feature off) it is exactly the weighted RRF;
    // regardless of backend, the *score given the reported ranks* must equal
    // the spec formula — verified separately below. Here, when the local path
    // is active, full equality holds.
    for (lw, sw) in [(1.0f32, 1.0), (2.0, 1.0), (1.0, 3.0), (0.5, 0.5)] {
        for k in [1u32, 60] {
            for (label, lex, sem) in corpus() {
                let svc = HybridSearchService::new()
                    .with_rrf_k(k)
                    .with_rrf_weights(lw, sw)
                    .with_mode(SearchMode::Hybrid);
                let got = svc.fuse(&lex, &sem, usize::MAX);
                let expected = reference_rrf(&lex, &sem, k, lw, sw);

                // Backend-agnostic invariant: for every returned hit, its score
                // equals the spec formula evaluated at its reported ranks.
                for hit in &got {
                    let mut want = 0.0f32;
                    if let Some(r) = hit.lexical_rank {
                        if lw > 0.0 {
                            want += lw / (k as f32 + r as f32 + 1.0);
                        }
                    }
                    if let Some(r) = hit.semantic_rank {
                        if sw > 0.0 {
                            want += sw / (k as f32 + r as f32 + 1.0);
                        }
                    }
                    assert!(
                        (hit.score - want).abs() <= EPS,
                        "{label} (k={k}, w=({lw},{sw})): id {} score {} != spec {want}",
                        hit.id,
                        hit.score
                    );
                }

                // Same id set as the reference.
                let got_ids: std::collections::BTreeSet<u64> = got.iter().map(|h| h.id).collect();
                let exp_ids: std::collections::BTreeSet<u64> =
                    expected.iter().map(|h| h.id).collect();
                assert_eq!(
                    got_ids, exp_ids,
                    "{label} (k={k}, w=({lw},{sw})): fused id set diverges from spec"
                );
            }
        }
    }
}

// =============================================================================
// 2. Named RRF spec properties (oracle-free)
// =============================================================================

#[test]
fn rrf_preserves_full_union_of_ids() {
    for (label, lex, sem) in corpus() {
        let mut want: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        want.extend(lex.iter().map(|&(id, _)| id));
        want.extend(sem.iter().map(|&(id, _)| id));
        let got: std::collections::BTreeSet<u64> =
            rrf_fuse(&lex, &sem, 60).iter().map(|h| h.id).collect();
        assert_eq!(
            got, want,
            "{label}: fused output must be exactly the id union"
        );
    }
}

#[test]
fn rrf_output_is_sorted_descending_with_id_tiebreak() {
    for k in [1u32, 60] {
        for (label, lex, sem) in corpus() {
            let fused = rrf_fuse(&lex, &sem, k);
            for w in fused.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                let ordered =
                    a.score > b.score || ((a.score - b.score).abs() <= EPS && a.id < b.id);
                assert!(
                    ordered,
                    "{label} (k={k}): ordering violated between id {} ({}) and id {} ({})",
                    a.id, a.score, b.id, b.score
                );
            }
        }
    }
}

#[test]
fn rrf_is_deterministic() {
    let (_, lex, sem) = &corpus()[6]; // partial_overlap
    let a = rrf_fuse(lex, sem, 60);
    let b = rrf_fuse(lex, sem, 60);
    assert_fused_eq("determinism", &a, &b);
}

#[test]
fn rrf_overlap_beats_single_lane_at_equal_rank() {
    // An item appearing in both lanes at rank r must score strictly higher than
    // an item appearing in only one lane at the same rank r (positive weights).
    let lex = vec![(1u64, 9.0), (2, 8.0)];
    let sem = vec![(1u64, 0.9), (3, 0.8)];
    let fused = rrf_fuse(&lex, &sem, 60);
    let by_id: BTreeMap<u64, f32> = fused.iter().map(|h| (h.id, h.score)).collect();
    // id 1: both lanes at rank 0. id 2: lexical rank 1 only. id 3: semantic rank 1 only.
    assert!(
        by_id[&1] > by_id[&2] && by_id[&1] > by_id[&3],
        "overlapping doc must outrank single-lane docs: {by_id:?}"
    );
    assert_eq!(fused[0].id, 1, "overlapping doc should be top-ranked");
}

#[test]
fn rrf_rank_contribution_is_strictly_monotonic() {
    // Within a single lane, an earlier rank yields a strictly larger score.
    let lex: Vec<(u64, f32)> = (0..6u64).map(|i| (i, 6.0 - i as f32)).collect();
    let fused = rrf_fuse(&lex, &[], 60);
    // Single lane => fused order equals input order, scores strictly decreasing.
    for w in fused.windows(2) {
        assert!(
            w[0].score > w[1].score,
            "single-lane RRF must be strictly decreasing: {} !> {}",
            w[0].score,
            w[1].score
        );
    }
    assert_eq!(
        fused.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5]
    );
}

#[test]
fn rrf_larger_k_compresses_score_gaps() {
    // The k parameter flattens the curve: the ratio between the rank-0 and
    // rank-1 contributions shrinks toward 1 as k grows.
    let lex = vec![(1u64, 1.0), (2, 1.0)];
    let ratio = |k: u32| {
        let f = rrf_fuse(&lex, &[], k);
        let s0 = f.iter().find(|h| h.id == 1).unwrap().score;
        let s1 = f.iter().find(|h| h.id == 2).unwrap().score;
        s0 / s1
    };
    let r_small = ratio(1); // (1/2)/(1/3) = 1.5
    let r_large = ratio(1000); // ~ (1/1001)/(1/1002) ≈ 1.001
    assert!(
        r_small > r_large,
        "larger k must compress gaps: ratio(1)={r_small} should exceed ratio(1000)={r_large}"
    );
    assert!(
        (r_small - 1.5).abs() < 1e-4,
        "k=1 ratio should be 1.5, got {r_small}"
    );
}

#[test]
fn rrf_zero_weight_lane_stamps_rank_but_adds_no_score() {
    // Verified via the service, which exposes weighting. A zero-weight lexical
    // lane must still populate lexical_rank, but a doc present only in that lane
    // gets score 0 from it.
    let lex = vec![(10u64, 9.0), (11, 8.0)];
    let sem = vec![(11u64, 0.9), (12, 0.8)];
    let svc = HybridSearchService::new()
        .with_rrf_k(60)
        .with_rrf_weights(0.0, 1.0)
        .with_mode(SearchMode::Hybrid);
    let fused = svc.fuse(&lex, &sem, usize::MAX);
    let by_id: BTreeMap<u64, &FusedResult> = fused.iter().map(|h| (h.id, h)).collect();

    // id 10 is lexical-only with zero lexical weight => score 0 but rank stamped.
    let h10 = by_id[&10];
    assert_eq!(
        h10.lexical_rank,
        Some(0),
        "rank must be stamped even at weight 0"
    );
    assert!(
        h10.score.abs() <= EPS,
        "zero-weight-only doc must score 0, got {}",
        h10.score
    );

    // id 11 is in both lanes: only the semantic contribution counts.
    let h11 = by_id[&11];
    let want11 = 1.0 / (60.0 + 0.0 + 1.0); // semantic rank 0
    assert!(
        (h11.score - want11).abs() <= EPS,
        "id 11 score {} != {want11}",
        h11.score
    );
}

// =============================================================================
// 3. SearchMode passthrough conformance
// =============================================================================

#[test]
fn lexical_mode_is_exact_passthrough() {
    let lex = vec![(1u64, 9.0), (2, 8.0), (3, 7.0)];
    let sem = vec![(9u64, 0.9)];
    let svc = HybridSearchService::new().with_mode(SearchMode::Lexical);
    let out = svc.fuse(&lex, &sem, 10);
    assert_eq!(out.len(), 3);
    for (rank, (h, &(id, score))) in out.iter().zip(lex.iter()).enumerate() {
        assert_eq!(h.id, id);
        assert!((h.score - score).abs() <= EPS);
        assert_eq!(h.lexical_rank, Some(rank));
        assert_eq!(
            h.semantic_rank, None,
            "lexical mode must ignore semantic lane"
        );
    }
}

#[test]
fn semantic_mode_is_exact_passthrough() {
    let lex = vec![(1u64, 9.0)];
    let sem = vec![(7u64, 0.9), (8, 0.8)];
    let svc = HybridSearchService::new().with_mode(SearchMode::Semantic);
    let out = svc.fuse(&lex, &sem, 10);
    assert_eq!(out.len(), 2);
    for (rank, (h, &(id, score))) in out.iter().zip(sem.iter()).enumerate() {
        assert_eq!(h.id, id);
        assert!((h.score - score).abs() <= EPS);
        assert_eq!(h.semantic_rank, Some(rank));
        assert_eq!(
            h.lexical_rank, None,
            "semantic mode must ignore lexical lane"
        );
    }
}

#[test]
fn top_k_truncation_keeps_highest_ranked_prefix() {
    let lex: Vec<(u64, f32)> = (0..20u64).map(|i| (i, 20.0 - i as f32)).collect();
    let sem: Vec<(u64, f32)> = (10..30u64).map(|i| (i, 1.0 - i as f32 / 100.0)).collect();
    let svc = HybridSearchService::new()
        .with_mode(SearchMode::Hybrid)
        .with_rrf_k(60);
    // Backend-agnostic: top_k must be a prefix of the service's own full ranking.
    let full = svc.fuse(&lex, &sem, usize::MAX);
    let truncated = svc.fuse(&lex, &sem, 5);
    assert_eq!(truncated.len(), 5, "top_k must bound result count");
    let full_ids: Vec<u64> = full.iter().take(5).map(|h| h.id).collect();
    let trunc_ids: Vec<u64> = truncated.iter().map(|h| h.id).collect();
    assert_eq!(
        trunc_ids, full_ids,
        "top_k prefix must match full ranking prefix"
    );
}

// =============================================================================
// 4. Two-tier blend conformance
// =============================================================================

fn fr(id: u64, score: f32) -> FusedResult {
    FusedResult {
        id,
        score,
        lexical_rank: None,
        semantic_rank: None,
    }
}

#[test]
fn blend_two_tier_scales_by_alpha_and_dedups() {
    let tier1 = vec![fr(1, 1.0), fr(2, 0.8), fr(3, 0.6)];
    let tier2 = vec![fr(3, 0.9), fr(4, 0.7), fr(5, 0.5)];
    let alpha = 0.7f32;
    let (out, metrics) = blend_two_tier(&tier1, &tier2, 10, alpha);

    // Tier1 fully taken first, scaled by alpha; tier2 fills remainder (minus dup
    // id 3) scaled by (1-alpha).
    let by_id: BTreeMap<u64, f32> = out.iter().map(|h| (h.id, h.score)).collect();
    assert!((by_id[&1] - 1.0 * alpha).abs() <= EPS);
    assert!((by_id[&2] - 0.8 * alpha).abs() <= EPS);
    assert!(
        (by_id[&3] - 0.6 * alpha).abs() <= EPS,
        "dup id keeps tier1 (alpha-scaled) score"
    );
    assert!((by_id[&4] - 0.7 * (1.0 - alpha)).abs() <= EPS);
    assert!((by_id[&5] - 0.5 * (1.0 - alpha)).abs() <= EPS);

    // id 3 must appear exactly once (dedup).
    assert_eq!(out.iter().filter(|h| h.id == 3).count(), 1);
    assert_eq!(metrics.tier1_count, 3);
    assert_eq!(metrics.tier2_count, 2);
    assert_eq!(metrics.overlap_count, 1, "id 3 overlaps both tiers");
}

#[test]
fn blend_two_tier_respects_top_k_bound() {
    let tier1 = vec![fr(1, 1.0), fr(2, 0.9), fr(3, 0.8)];
    let tier2 = vec![fr(4, 0.7), fr(5, 0.6)];
    let (out, metrics) = blend_two_tier(&tier1, &tier2, 2, 0.5);
    assert_eq!(out.len(), 2, "top_k must cap output");
    // Tier1 fills first, so the cap is satisfied entirely by tier1.
    assert_eq!(metrics.tier1_count, 2);
    assert_eq!(metrics.tier2_count, 0);
    assert_eq!(out.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 2]);
}

#[test]
fn blend_alpha_is_clamped_to_unit_interval() {
    let tier1 = vec![fr(1, 1.0)];
    let tier2 = vec![fr(2, 1.0)];
    // alpha > 1 clamps to 1.0 -> tier1 unscaled, tier2 * 0.
    let (out, _) = blend_two_tier(&tier1, &tier2, 10, 5.0);
    let by_id: BTreeMap<u64, f32> = out.iter().map(|h| (h.id, h.score)).collect();
    assert!(
        (by_id[&1] - 1.0).abs() <= EPS,
        "alpha clamped to 1: tier1 unscaled"
    );
    assert!(
        by_id[&2].abs() <= EPS,
        "alpha clamped to 1: tier2 scaled by 0"
    );

    // alpha < 0 clamps to 0.0 -> tier1 * 0, tier2 unscaled.
    let (out2, _) = blend_two_tier(&tier1, &tier2, 10, -3.0);
    let by_id2: BTreeMap<u64, f32> = out2.iter().map(|h| (h.id, h.score)).collect();
    assert!(
        by_id2[&1].abs() <= EPS,
        "alpha clamped to 0: tier1 scaled by 0"
    );
    assert!(
        (by_id2[&2] - 1.0).abs() <= EPS,
        "alpha clamped to 0: tier2 unscaled"
    );
}

#[test]
fn blend_rank_correlation_equals_kendall_tau_of_tiers() {
    let tier1 = vec![fr(1, 1.0), fr(2, 0.9), fr(3, 0.8), fr(4, 0.7)];
    let tier2 = vec![fr(1, 1.0), fr(2, 0.9), fr(4, 0.8), fr(3, 0.7)];
    let (_, metrics) = blend_two_tier(&tier1, &tier2, 10, 0.5);
    let t1_ids: Vec<u64> = tier1.iter().map(|h| h.id).collect();
    let t2_ids: Vec<u64> = tier2.iter().map(|h| h.id).collect();
    let expected = kendall_tau(&t1_ids, &t2_ids);
    assert!(
        (metrics.rank_correlation - expected).abs() <= EPS,
        "metrics.rank_correlation {} must equal kendall_tau {}",
        metrics.rank_correlation,
        expected
    );
}

// =============================================================================
// 5. Kendall tau conformance
// =============================================================================

/// Independent O(n^2) Kendall tau over the common-id subset, mirroring the spec
/// in `hybrid_search::kendall_tau` (concordant - discordant) / (C + D).
fn reference_kendall(a: &[u64], b: &[u64]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let rank_a: BTreeMap<u64, usize> = a.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let rank_b: BTreeMap<u64, usize> = b.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let common: Vec<u64> = a
        .iter()
        .copied()
        .filter(|id| rank_b.contains_key(id))
        .collect();
    let n = common.len();
    if n < 2 {
        return 0.0;
    }
    let (mut c, mut d) = (0i64, 0i64);
    for i in 0..n {
        for j in (i + 1)..n {
            let ord_a = rank_a[&common[i]] as i64 - rank_a[&common[j]] as i64;
            let ord_b = rank_b[&common[i]] as i64 - rank_b[&common[j]] as i64;
            let prod = ord_a * ord_b;
            if prod > 0 {
                c += 1;
            } else if prod < 0 {
                d += 1;
            }
        }
    }
    if c + d == 0 {
        return 0.0;
    }
    (c - d) as f32 / (c + d) as f32
}

#[test]
fn kendall_tau_perfect_agreement_is_one() {
    let r = vec![1u64, 2, 3, 4, 5];
    assert!((kendall_tau(&r, &r) - 1.0).abs() <= EPS);
}

#[test]
fn kendall_tau_perfect_reversal_is_minus_one() {
    let a = vec![1u64, 2, 3, 4, 5];
    let b = vec![5u64, 4, 3, 2, 1];
    assert!((kendall_tau(&a, &b) + 1.0).abs() <= EPS);
}

#[test]
fn kendall_tau_single_swap_value() {
    // Swapping one adjacent pair among 4 items: C=5, D=1 => tau = 4/6 = 0.6667.
    let a = vec![1u64, 2, 3, 4];
    let b = vec![2u64, 1, 3, 4];
    let tau = kendall_tau(&a, &b);
    assert!(
        (tau - (4.0 / 6.0)).abs() <= EPS,
        "single adjacent swap tau should be ~0.6667, got {tau}"
    );
}

#[test]
fn kendall_tau_empty_and_singleton_are_zero() {
    assert_eq!(kendall_tau(&[], &[]), 0.0);
    assert_eq!(
        kendall_tau(&[1], &[1]),
        0.0,
        "fewer than 2 common items => 0"
    );
    assert_eq!(kendall_tau(&[1, 2, 3], &[]), 0.0);
    assert_eq!(kendall_tau(&[1], &[2]), 0.0, "no common items => 0");
}

#[test]
fn kendall_tau_matches_reference_oracle() {
    let cases: &[(&[u64], &[u64])] = &[
        (&[1, 2, 3, 4, 5], &[1, 3, 2, 5, 4]),
        (&[1, 2, 3, 4], &[4, 1, 2, 3]),
        (&[1, 2, 3, 4, 5, 6], &[6, 5, 4, 3, 2, 1]),
        (&[10, 20, 30], &[30, 10, 20]),
        (&[1, 2, 3, 4, 5], &[5, 6, 7, 1, 2]), // partial overlap {1,2,5}
        (&[1, 2, 3], &[3, 2, 1]),
    ];
    for (a, b) in cases {
        let got = kendall_tau(a, b);
        let want = reference_kendall(a, b);
        assert!(
            (got - want).abs() <= EPS,
            "kendall_tau({a:?},{b:?}) = {got} but oracle = {want}"
        );
        assert!((-1.0..=1.0).contains(&got), "tau out of [-1,1]: {got}");
    }
}
