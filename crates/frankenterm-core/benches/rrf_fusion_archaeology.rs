//! Criterion comparison for the `rrf_fuse` hot path against the
//! pre-`95d5bf42` implementation recovered via git archaeology.

use std::collections::{HashMap, HashSet};
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::search::{FusedResult, rrf_fuse};

fn make_ranked_list(size: usize) -> Vec<(u64, f32)> {
    (0..size)
        .map(|i| (i as u64, 1.0 - (i as f32 / size as f32)))
        .collect()
}

fn dedupe_ranked_hits_old(items: &[(u64, f32)]) -> Vec<(u64, f32)> {
    let mut seen = HashSet::with_capacity(items.len());
    let mut deduped = Vec::with_capacity(items.len());
    for &(id, score) in items {
        if seen.insert(id) {
            deduped.push((id, score));
        }
    }
    deduped
}

fn rrf_component_score_old(rank: usize, k: u32, weight: f32) -> f32 {
    if weight <= 0.0 {
        return 0.0;
    }
    weight / (k as f32 + rank as f32 + 1.0)
}

/// Baseline copied from `crates/frankenterm-core/src/search/hybrid_search.rs`
/// at `95d5bf42^`, before the score-table preallocation and loop hoists landed.
fn rrf_fuse_old(lexical: &[(u64, f32)], semantic: &[(u64, f32)], k: u32) -> Vec<FusedResult> {
    let lexical = dedupe_ranked_hits_old(lexical);
    let semantic = dedupe_ranked_hits_old(semantic);
    let mut scores: HashMap<u64, (f32, Option<usize>, Option<usize>)> = HashMap::new();

    for (rank, &(id, _score)) in lexical.iter().enumerate() {
        let entry = scores.entry(id).or_insert((0.0, None, None));
        entry.0 += rrf_component_score_old(rank, k, 1.0);
        entry.1 = Some(rank);
    }

    for (rank, &(id, _score)) in semantic.iter().enumerate() {
        let entry = scores.entry(id).or_insert((0.0, None, None));
        entry.0 += rrf_component_score_old(rank, k, 1.0);
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

fn assert_equivalent(current: &[FusedResult], old: &[FusedResult]) {
    assert_eq!(current.len(), old.len(), "fused result length changed");
    for (lhs, rhs) in current.iter().zip(old.iter()) {
        assert_eq!(lhs.id, rhs.id, "result ids diverged");
        assert_eq!(
            lhs.lexical_rank, rhs.lexical_rank,
            "lexical ranks diverged for id {}",
            lhs.id
        );
        assert_eq!(
            lhs.semantic_rank, rhs.semantic_rank,
            "semantic ranks diverged for id {}",
            lhs.id
        );
        assert!(
            (lhs.score - rhs.score).abs() <= f32::EPSILON,
            "scores diverged for id {}: {} vs {}",
            lhs.id,
            lhs.score,
            rhs.score
        );
    }
}

fn bench_rrf_fusion_archaeology(c: &mut Criterion) {
    let mut group = c.benchmark_group("rrf_fusion_archaeology");
    group.sample_size(30);

    for size in [10usize, 50, 100, 500, 1000] {
        let lexical = make_ranked_list(size);
        let semantic_rev: Vec<(u64, f32)> = make_ranked_list(size).into_iter().rev().collect();
        assert_equivalent(
            &rrf_fuse(&lexical, &semantic_rev, 60),
            &rrf_fuse_old(&lexical, &semantic_rev, 60),
        );

        group.bench_with_input(
            BenchmarkId::new("current_equal_size", size),
            &size,
            |b, _| {
                b.iter(|| rrf_fuse(black_box(&lexical), black_box(&semantic_rev), 60));
            },
        );
        group.bench_with_input(BenchmarkId::new("old_equal_size", size), &size, |b, _| {
            b.iter(|| rrf_fuse_old(black_box(&lexical), black_box(&semantic_rev), 60));
        });
    }

    let lex_1000 = make_ranked_list(1000);
    let sem_10 = make_ranked_list(10);
    assert_equivalent(
        &rrf_fuse(&lex_1000, &sem_10, 60),
        &rrf_fuse_old(&lex_1000, &sem_10, 60),
    );

    group.bench_function("current_asymmetric_1000x10", |b| {
        b.iter(|| rrf_fuse(black_box(&lex_1000), black_box(&sem_10), 60));
    });
    group.bench_function("old_asymmetric_1000x10", |b| {
        b.iter(|| rrf_fuse_old(black_box(&lex_1000), black_box(&sem_10), 60));
    });

    group.finish();
}

criterion_group!(benches, bench_rrf_fusion_archaeology);
criterion_main!(benches);
