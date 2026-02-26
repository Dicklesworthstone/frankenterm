//! FrankenSearch performance benchmark suite (ft-dr6zv.1.7).
//!
//! Criterion benchmarks for the search stack:
//! - RRF fusion latency across input sizes
//! - Two-tier blending latency
//! - Kendall Tau computation
//! - Indexing throughput (documents per second)
//! - Content hash deduplication overhead

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::search::{
    FusedResult, IndexableDocument, IndexingConfig, SearchDocumentSource, SearchIndex,
    blend_two_tier, kendall_tau, rrf_fuse,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_ranked_list(size: usize) -> Vec<(u64, f32)> {
    (0..size)
        .map(|i| (i as u64, 1.0 - (i as f32 / size as f32)))
        .collect()
}

fn make_fused_results(size: usize) -> Vec<FusedResult> {
    (0..size)
        .map(|i| FusedResult {
            id: i as u64,
            score: 1.0 - (i as f32 / size as f32),
            lexical_rank: Some(i),
            semantic_rank: Some(size - 1 - i),
        })
        .collect()
}

fn make_ranking(size: usize) -> Vec<u64> {
    (0..size).map(|i| i as u64).collect()
}

fn make_indexable_docs(count: usize) -> Vec<IndexableDocument> {
    (0..count)
        .map(|i| {
            IndexableDocument::text(
                SearchDocumentSource::Scrollback,
                format!(
                    "Benchmark document {i}: terminal output line with sufficient content for realistic sizing"
                ),
                i as i64 * 100,
                Some(0),
                None,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Benchmark: RRF Fusion
// ---------------------------------------------------------------------------

fn bench_rrf_fusion(c: &mut Criterion) {
    let mut group = c.benchmark_group("rrf_fusion");

    for size in [10, 50, 100, 500, 1000] {
        let lexical = make_ranked_list(size);
        let semantic = make_ranked_list(size);
        // Reverse semantic to create interesting overlap patterns.
        let semantic_rev: Vec<(u64, f32)> =
            semantic.iter().rev().copied().collect();

        group.bench_with_input(
            BenchmarkId::new("equal_size", size),
            &size,
            |b, _| {
                b.iter(|| rrf_fuse(black_box(&lexical), black_box(&semantic_rev), 60));
            },
        );
    }

    // Asymmetric: large lexical, small semantic.
    let lex_1000 = make_ranked_list(1000);
    let sem_10 = make_ranked_list(10);
    group.bench_function("asymmetric_1000x10", |b| {
        b.iter(|| rrf_fuse(black_box(&lex_1000), black_box(&sem_10), 60));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Two-Tier Blending
// ---------------------------------------------------------------------------

fn bench_two_tier_blending(c: &mut Criterion) {
    let mut group = c.benchmark_group("two_tier_blend");

    for size in [10, 50, 100, 500] {
        let tier1 = make_fused_results(size);
        let tier2 = make_fused_results(size);

        group.bench_with_input(
            BenchmarkId::new("top_k_equals_size", size),
            &size,
            |b, &sz| {
                b.iter(|| blend_two_tier(black_box(&tier1), black_box(&tier2), sz, 0.7));
            },
        );
    }

    // Benchmark with varying alpha values.
    let t1 = make_fused_results(100);
    let t2 = make_fused_results(100);
    for alpha_x10 in [0, 3, 5, 7, 10] {
        let alpha = alpha_x10 as f32 / 10.0;
        group.bench_with_input(
            BenchmarkId::new("alpha_sweep", format!("{alpha:.1}")),
            &alpha,
            |b, &a| {
                b.iter(|| blend_two_tier(black_box(&t1), black_box(&t2), 50, a));
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Kendall Tau
// ---------------------------------------------------------------------------

fn bench_kendall_tau(c: &mut Criterion) {
    let mut group = c.benchmark_group("kendall_tau");

    for size in [10, 50, 100, 500, 1000] {
        let ranking_a = make_ranking(size);
        let ranking_b: Vec<u64> = ranking_a.iter().rev().copied().collect();

        group.bench_with_input(BenchmarkId::new("reversed", size), &size, |b, _| {
            b.iter(|| kendall_tau(black_box(&ranking_a), black_box(&ranking_b)));
        });
    }

    // Identical rankings (best case).
    let ranking = make_ranking(500);
    group.bench_function("identical_500", |b| {
        b.iter(|| kendall_tau(black_box(&ranking), black_box(&ranking)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Indexing Throughput
// ---------------------------------------------------------------------------

fn bench_indexing_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("indexing_throughput");
    group.sample_size(20); // Fewer samples since each involves I/O.

    for doc_count in [10, 50, 100] {
        let docs = make_indexable_docs(doc_count);

        group.bench_with_input(
            BenchmarkId::new("ingest", doc_count),
            &doc_count,
            |b, _| {
                b.iter_with_setup(
                    || {
                        let tmp = tempfile::tempdir().unwrap();
                        let config = IndexingConfig {
                            index_dir: tmp.path().to_path_buf(),
                            max_index_size_bytes: 100 * 1024 * 1024,
                            ttl_days: 30,
                            flush_interval_secs: 60,
                            flush_docs_threshold: 1000,
                            max_docs_per_second: 100_000,
                        };
                        let index = SearchIndex::open(config).unwrap();
                        (tmp, index)
                    },
                    |(_tmp, mut index)| {
                        let _ = index.ingest_documents(
                            black_box(&docs),
                            black_box(99999),
                            false,
                            None,
                        );
                    },
                );
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Content Hash Dedup
// ---------------------------------------------------------------------------

fn bench_dedup_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("dedup_overhead");
    group.sample_size(20);

    // Measure the overhead of duplicate detection with growing index.
    for existing_count in [0, 100, 500] {
        group.bench_with_input(
            BenchmarkId::new("with_existing_docs", existing_count),
            &existing_count,
            |b, &count| {
                b.iter_with_setup(
                    || {
                        let tmp = tempfile::tempdir().unwrap();
                        let config = IndexingConfig {
                            index_dir: tmp.path().to_path_buf(),
                            max_index_size_bytes: 100 * 1024 * 1024,
                            ttl_days: 30,
                            flush_interval_secs: 60,
                            flush_docs_threshold: 10000,
                            max_docs_per_second: 100_000,
                        };
                        let mut index = SearchIndex::open(config).unwrap();
                        // Pre-populate with existing docs.
                        let existing = make_indexable_docs(count);
                        let _ = index.ingest_documents(&existing, 0, false, None);
                        let new_docs = make_indexable_docs(10);
                        (tmp, index, new_docs)
                    },
                    |(_tmp, mut index, new_docs)| {
                        let _ = index.ingest_documents(
                            black_box(&new_docs),
                            black_box(99999),
                            false,
                            None,
                        );
                    },
                );
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion groups
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_rrf_fusion,
    bench_two_tier_blending,
    bench_kendall_tau,
    bench_indexing_throughput,
    bench_dedup_overhead,
);
criterion_main!(benches);
