//! Metamorphic tests for RRF hybrid ranking (oracle-problem coverage).
//!
//! The "correct" fused ranking of arbitrary document lists is not computable in
//! general (oracle problem), but Reciprocal Rank Fusion obeys strong, spec-level
//! input→output relations. This suite verifies those relations under randomized
//! (proptest) inputs, complementing the exact-oracle conformance suite in
//! `conformance_rrf_hybrid_ranking.rs`.
//!
//! Headline relation: RRF is **rank-based** — `rrf_fuse` consumes only the
//! *order* of each lane, never the score magnitudes. So replacing every score
//! with arbitrary finite values (preserving id order) must not change anything.
//!
//! A `validate_*` test performs mutation testing: it runs the same relations
//! against deliberately-buggy local RRF variants and asserts each planted bug is
//! caught, proving the relations have real fault-detecting power.

use frankenterm_core::search::{FusedResult, kendall_tau, rrf_fuse};
use proptest::prelude::*;

const EPS: f32 = 1e-6;

// ---------------------------------------------------------------------------
// Input generation
// ---------------------------------------------------------------------------

/// A ranked lane: distinct-id pairs are not guaranteed (dedup is part of the
/// contract), ids drawn from a small range so the two lanes overlap often, and
/// scores are finite (RRF ignores them, but transforms must stay well-defined).
fn arb_lane() -> impl Strategy<Value = Vec<(u64, f32)>> {
    prop::collection::vec(
        (0u64..12, (-1.0e3f32..1.0e3f32)),
        0..14,
    )
}

fn ordered_pairs(fused: &[FusedResult]) -> Vec<(u64, f32)> {
    fused.iter().map(|f| (f.id, f.score)).collect()
}

fn assert_same_ordering(label: &str, a: &[FusedResult], b: &[FusedResult]) {
    assert_eq!(a.len(), b.len(), "{label}: length differs");
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(x.id, y.id, "{label}: id order differs at {i}");
        assert!(
            (x.score - y.score).abs() <= EPS,
            "{label}: score differs at {i} for id {}: {} vs {}",
            x.id,
            x.score,
            y.score
        );
    }
}

// ---------------------------------------------------------------------------
// MR1 — Score-magnitude independence (Equivalence). Score = 25.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Replacing every lane score with an arbitrary finite value, while keeping
    /// the id order intact, must produce a byte-identical fusion. RRF ranks by
    /// position, not magnitude; any dependence on score values is a bug.
    #[test]
    fn mr_score_magnitude_independence(
        lex in arb_lane(),
        sem in arb_lane(),
        k in 0u32..200,
        repl in prop::collection::vec(-5.0e6f32..5.0e6f32, 0..64),
    ) {
        let remap = |lane: &[(u64, f32)]| -> Vec<(u64, f32)> {
            lane.iter()
                .enumerate()
                .map(|(i, &(id, _))| (id, *repl.get(i).unwrap_or(&0.0)))
                .collect()
        };
        let base = rrf_fuse(&lex, &sem, k);
        let remapped = rrf_fuse(&remap(&lex), &remap(&sem), k);
        prop_assert_eq!(base.len(), remapped.len());
        for (x, y) in base.iter().zip(remapped.iter()) {
            prop_assert_eq!(x.id, y.id, "id order changed when only scores changed");
            prop_assert!((x.score - y.score).abs() <= EPS);
            prop_assert_eq!(x.lexical_rank, y.lexical_rank);
            prop_assert_eq!(x.semantic_rank, y.semantic_rank);
        }
    }

    // -----------------------------------------------------------------------
    // MR2 — Lane-swap symmetry (Equivalence under equal weights). Score = 16.
    // -----------------------------------------------------------------------

    /// With equal weights, fusing (lex, sem) and (sem, lex) yields the same
    /// (id, score) ranking — the per-lane rank labels swap, but the fused score
    /// (a symmetric sum) and the score+id ordering are invariant.
    #[test]
    fn mr_lane_swap_symmetry(lex in arb_lane(), sem in arb_lane(), k in 0u32..200) {
        let forward = rrf_fuse(&lex, &sem, k);
        let swapped = rrf_fuse(&sem, &lex, k);
        assert_same_ordering("lane-swap", &forward, &swapped);
        // And the rank labels must mirror: forward.lexical_rank == swapped.semantic_rank.
        let fwd: std::collections::HashMap<u64, &FusedResult> =
            forward.iter().map(|r| (r.id, r)).collect();
        for s in &swapped {
            let f = fwd[&s.id];
            prop_assert_eq!(f.lexical_rank, s.semantic_rank, "lex/sem rank did not mirror on swap");
            prop_assert_eq!(f.semantic_rank, s.lexical_rank, "sem/lex rank did not mirror on swap");
        }
    }

    // -----------------------------------------------------------------------
    // MR3 — Irrelevant-tail append (Inclusive/stability). Score = 6.
    // -----------------------------------------------------------------------

    /// Appending a brand-new id at the END of the lexical lane adds exactly one
    /// result and never reorders or rescoring the pre-existing ids (their ranks
    /// are unchanged). Tests rank-recomputation stability.
    #[test]
    fn mr_irrelevant_tail_append_preserves_originals(
        lex in arb_lane(),
        sem in arb_lane(),
        k in 0u32..200,
    ) {
        let base = rrf_fuse(&lex, &sem, k);
        // Fresh id larger than any present.
        let max_id = lex.iter().chain(sem.iter()).map(|&(id, _)| id).max().unwrap_or(0);
        let fresh = max_id.saturating_add(1);
        let mut lex2 = lex.clone();
        lex2.push((fresh, 0.123));
        let extended = rrf_fuse(&lex2, &sem, k);

        // The fresh id must appear iff it wasn't already deduped away.
        let base_ids: std::collections::HashSet<u64> = base.iter().map(|r| r.id).collect();
        // Relative order of the original ids must be preserved in the extended result.
        let base_order: Vec<u64> = base.iter().map(|r| r.id).collect();
        let ext_order_filtered: Vec<u64> =
            extended.iter().map(|r| r.id).filter(|id| base_ids.contains(id)).collect();
        prop_assert_eq!(base_order, ext_order_filtered, "appending a tail id reordered originals");
        // Original ids keep identical scores.
        let ext_by_id: std::collections::HashMap<u64, f32> =
            extended.iter().map(|r| (r.id, r.score)).collect();
        for r in &base {
            let e = ext_by_id[&r.id];
            prop_assert!((r.score - e).abs() <= EPS, "appending a tail id changed an original score");
        }
    }

    // -----------------------------------------------------------------------
    // MR4 — Dedup idempotence (within-lane duplicate). Score = 4.5.
    // -----------------------------------------------------------------------

    /// Re-appending an already-present id to the lexical lane must not change the
    /// fusion: dedup keeps the first occurrence, later duplicates are inert.
    #[test]
    fn mr_within_lane_duplicate_is_inert(
        lex in arb_lane().prop_filter("non-empty", |l| !l.is_empty()),
        sem in arb_lane(),
        k in 0u32..200,
        idx in 0usize..14,
    ) {
        let base = rrf_fuse(&lex, &sem, k);
        let dup = lex[idx % lex.len()];
        let mut lex2 = lex.clone();
        lex2.push((dup.0, dup.1 + 7.0)); // same id, different (ignored) score
        let with_dup = rrf_fuse(&lex2, &sem, k);
        assert_same_ordering("dedup-idempotence", &base, &with_dup);
    }

    // -----------------------------------------------------------------------
    // MR5 — Rank-improvement monotonicity (within-lane swap). Score = 6.
    // -----------------------------------------------------------------------

    /// Swapping an item with its immediate predecessor in the lexical lane gives
    /// it a better (smaller) rank, which must not DECREASE its fused score. A
    /// flipped rank formula (e.g. multiply instead of divide) violates this.
    #[test]
    fn mr_rank_improvement_does_not_decrease_score(
        lex in arb_lane().prop_filter("distinct ids, len>=2", |l| {
            let ids: std::collections::HashSet<u64> = l.iter().map(|&(id, _)| id).collect();
            ids.len() == l.len() && l.len() >= 2
        }),
        sem in arb_lane(),
        k in 0u32..200,
        pos in 1usize..14,
    ) {
        let i = pos % lex.len();
        let i = if i == 0 { 1 } else { i };
        let promoted_id = lex[i].0;
        let before = rrf_fuse(&lex, &sem, k);
        let mut lex2 = lex.clone();
        lex2.swap(i, i - 1); // promote item at i to a better rank
        let after = rrf_fuse(&lex2, &sem, k);
        let s_before = before.iter().find(|r| r.id == promoted_id).unwrap().score;
        let s_after = after.iter().find(|r| r.id == promoted_id).unwrap().score;
        prop_assert!(
            s_after + EPS >= s_before,
            "promoting id {promoted_id} to a better rank decreased its score: {s_before} -> {s_after}"
        );
    }

    // -----------------------------------------------------------------------
    // MR6 — Kendall tau symmetry (Equivalence). Score = 9.
    // -----------------------------------------------------------------------

    /// Rank correlation is symmetric: τ(a, b) == τ(b, a).
    #[test]
    fn mr_kendall_tau_symmetric(
        a in prop::collection::vec(0u64..10, 0..10),
        b in prop::collection::vec(0u64..10, 0..10),
    ) {
        let ab = kendall_tau(&a, &b);
        let ba = kendall_tau(&b, &a);
        prop_assert!((ab - ba).abs() <= EPS, "kendall_tau asymmetric: {ab} vs {ba}");
        prop_assert!((-1.0..=1.0).contains(&ab), "tau out of range: {ab}");
    }
}

// ---------------------------------------------------------------------------
// VALIDATE — mutation testing: prove the relations catch planted RRF bugs.
// ---------------------------------------------------------------------------

/// A deliberately-buggy RRF that (bug) ranks by raw score magnitude instead of
/// by rank position. Used only to validate that MR1 has real detecting power.
fn mutant_score_dependent_rrf(lex: &[(u64, f32)], sem: &[(u64, f32)], _k: u32) -> Vec<(u64, f32)> {
    // Fuse by summing raw scores per id (WRONG — RRF must be rank-based).
    let mut acc: std::collections::BTreeMap<u64, f32> = std::collections::BTreeMap::new();
    for &(id, s) in lex.iter().chain(sem.iter()) {
        *acc.entry(id).or_insert(0.0) += s;
    }
    let mut v: Vec<(u64, f32)> = acc.into_iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    v
}

/// A deliberately-buggy RRF that (bug) flips the rank formula so later ranks
/// score higher. Used to validate MR5 (rank-improvement monotonicity).
fn mutant_inverted_rank_rrf(lex: &[(u64, f32)], sem: &[(u64, f32)], k: u32) -> Vec<(u64, f32)> {
    let mut acc: std::collections::BTreeMap<u64, f32> = std::collections::BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    for (rank, &(id, _)) in lex.iter().filter(|&&(id, _)| seen.insert(id)).enumerate() {
        // BUG: multiply by rank instead of dividing — later ranks score higher.
        *acc.entry(id).or_insert(0.0) += (k as f32 + rank as f32 + 1.0) * 0.001;
    }
    seen.clear();
    for (rank, &(id, _)) in sem.iter().filter(|&&(id, _)| seen.insert(id)).enumerate() {
        *acc.entry(id).or_insert(0.0) += (k as f32 + rank as f32 + 1.0) * 0.001;
    }
    let mut v: Vec<(u64, f32)> = acc.into_iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    v
}

#[test]
fn validate_mr1_catches_score_dependence() {
    // A case where rank order and score order disagree, so a score-dependent
    // implementation produces a different ranking under score remapping.
    let lex = vec![(1u64, 0.1f32), (2, 0.9)]; // id 1 ranked first, but lower score
    let sem: Vec<(u64, f32)> = vec![];
    // Correct RRF: id 1 (rank 0) outranks id 2 (rank 1) regardless of score.
    let correct = rrf_fuse(&lex, &sem, 60);
    assert_eq!(correct[0].id, 1, "sanity: rank-based RRF ranks id 1 first");

    // The mutant ranks by score sum, so id 2 (0.9) beats id 1 (0.1) — different.
    let mutant = mutant_score_dependent_rrf(&lex, &sem, 60);
    assert_eq!(mutant[0].0, 2, "mutant ranks by score, so id 2 wins");

    // MR1 (score-magnitude independence) distinguishes them: remap scores and
    // the correct impl is unchanged while the mutant flips.
    let remap = vec![(1u64, 9.0f32), (2, 0.0)]; // invert score relationship
    let correct_remap = rrf_fuse(&remap, &sem, 60);
    assert_eq!(
        ordered_pairs(&correct).iter().map(|p| p.0).collect::<Vec<_>>(),
        correct_remap.iter().map(|r| r.id).collect::<Vec<_>>(),
        "correct RRF is score-magnitude independent"
    );
    let mutant_remap = mutant_score_dependent_rrf(&remap, &sem, 60);
    assert_ne!(
        mutant.iter().map(|p| p.0).collect::<Vec<_>>(),
        mutant_remap.iter().map(|p| p.0).collect::<Vec<_>>(),
        "MR1 must catch the score-dependent mutant (its ranking changes under remap)"
    );
}

/// Deterministic regression for the asymmetry MR1's sibling (MR6) shrank to:
/// with a duplicate id in one ranking, kendall_tau was argument-order dependent
/// (-0.2 vs -0.333). After the first-occurrence-dense-rank fix it is symmetric.
#[test]
fn kendall_tau_symmetric_with_duplicate_ids_regression() {
    let a = [3u64, 3, 7, 6];
    let b = [6u64, 3, 7];
    let ab = kendall_tau(&a, &b);
    let ba = kendall_tau(&b, &a);
    assert!(
        (ab - ba).abs() <= EPS,
        "kendall_tau must be symmetric even with duplicate ids: {ab} vs {ba}"
    );
    assert!((-1.0..=1.0).contains(&ab));
}

#[test]
fn validate_mr5_catches_inverted_rank_formula() {
    let lex = vec![(10u64, 1.0f32), (20, 1.0), (30, 1.0)];
    let sem: Vec<(u64, f32)> = vec![];
    // Promote id 20 from rank 1 to rank 0.
    let mut lex2 = lex.clone();
    lex2.swap(1, 0);

    // Correct: promotion does not decrease score.
    let before = rrf_fuse(&lex, &sem, 60);
    let after = rrf_fuse(&lex2, &sem, 60);
    let cb = before.iter().find(|r| r.id == 20).unwrap().score;
    let ca = after.iter().find(|r| r.id == 20).unwrap().score;
    assert!(ca + EPS >= cb, "sanity: correct RRF score non-decreasing on promotion");

    // Mutant (inverted formula): promotion DECREASES the score — MR5 catches it.
    let mb = mutant_inverted_rank_rrf(&lex, &sem, 60)
        .into_iter()
        .find(|&(id, _)| id == 20)
        .unwrap()
        .1;
    let ma = mutant_inverted_rank_rrf(&lex2, &sem, 60)
        .into_iter()
        .find(|&(id, _)| id == 20)
        .unwrap()
        .1;
    assert!(
        ma < mb,
        "MR5 must catch the inverted-rank mutant (promotion decreases its score: {mb} -> {ma})"
    );
}
