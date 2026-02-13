//! Benchmarks for semantic output compression.
//!
//! Performance budgets:
//! - Template extraction (100 similar lines): **< 500μs**
//! - Compression throughput (mixed): **> 100K lines/sec**
//! - Decompression throughput: **> 200K lines/sec**
//! - Edit distance (50-char lines): **< 5μs**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::output_compression::{
    CompressionConfig, compress, compression_stats, decompress, edit_distance, extract_template,
    lines_similar,
};
use std::hint::black_box;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "semantic_compression_ratio_progress",
        budget: "ratio > 1.5 for 1000 progress-counter lines",
    },
    bench_common::BenchBudget {
        name: "semantic_compression_ratio_mixed",
        budget: "ratio > 1.2 for mixed agent output",
    },
    bench_common::BenchBudget {
        name: "semantic_compression_throughput",
        budget: "> 100K lines/sec for compression pipeline",
    },
    bench_common::BenchBudget {
        name: "semantic_decompression_throughput",
        budget: "> 200K lines/sec for decompression",
    },
    bench_common::BenchBudget {
        name: "semantic_template_extraction",
        budget: "p50 < 500us for 100-line group template extraction",
    },
];

// =============================================================================
// Compression ratio benchmarks
// =============================================================================

fn bench_compression_ratio_progress(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_compression_ratio_progress");

    for count in [100, 500, 1000] {
        let lines: Vec<String> = (1..=count)
            .map(|i| format!("Processing file {i}/{count}"))
            .collect();
        let input = lines.join("\n");
        let config = CompressionConfig::default();

        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("compress", count), &input, |b, input| {
            b.iter(|| {
                let compressed = compress(black_box(input), &config);
                black_box(compressed);
            });
        });
    }

    group.finish();
}

fn bench_compression_ratio_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_compression_ratio_mixed");

    let input = generate_mixed_agent_output(500);
    let config = CompressionConfig::default();

    group.throughput(Throughput::Bytes(input.len() as u64));
    group.bench_function("compress_mixed_500", |b| {
        b.iter(|| {
            let compressed = compress(black_box(&input), &config);
            black_box(compressed);
        });
    });

    group.finish();
}

// =============================================================================
// Throughput benchmarks
// =============================================================================

fn bench_compression_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_compression_throughput");

    for line_count in [100, 1000, 5000] {
        let input = generate_repetitive_output(line_count);
        let config = CompressionConfig::default();

        group.throughput(Throughput::Elements(line_count as u64));
        group.bench_with_input(
            BenchmarkId::new("compress", line_count),
            &input,
            |b, input| {
                b.iter(|| compress(black_box(input), &config));
            },
        );
    }

    group.finish();
}

fn bench_decompression_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_decompression_throughput");

    for line_count in [100, 1000, 5000] {
        let input = generate_repetitive_output(line_count);
        let config = CompressionConfig::default();
        let compressed = compress(&input, &config);

        group.throughput(Throughput::Elements(line_count as u64));
        group.bench_with_input(
            BenchmarkId::new("decompress", line_count),
            &compressed,
            |b, compressed| {
                b.iter(|| decompress(black_box(compressed)));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Template extraction benchmarks
// =============================================================================

fn bench_template_extraction(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_template_extraction");

    for group_size in [10, 50, 100] {
        let lines: Vec<String> = (1..=group_size)
            .map(|i| format!("Compiling crate {i}/{group_size} (frankenterm-core)"))
            .collect();
        let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();

        group.bench_with_input(
            BenchmarkId::new("extract", group_size),
            &line_refs,
            |b, lines| {
                b.iter(|| extract_template(black_box(lines)));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Edit distance benchmarks
// =============================================================================

fn bench_edit_distance(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_edit_distance");

    let pairs = [
        ("short", "Processing file 1/100", "Processing file 2/100"),
        (
            "medium",
            "warning: unused variable `x` in module foo::bar::baz at line 42",
            "warning: unused variable `y` in module foo::bar::baz at line 43",
        ),
        ("long", &"a".repeat(200), &"b".repeat(200)),
    ];

    for (name, a, b) in &pairs {
        group.bench_with_input(
            BenchmarkId::new("distance", name),
            &(a, b),
            |bench, (a, b)| {
                bench.iter(|| edit_distance(black_box(a.as_bytes()), black_box(b.as_bytes())));
            },
        );
    }

    // Similarity check (includes threshold comparison)
    group.bench_function("lines_similar_check", |b| {
        b.iter(|| {
            lines_similar(
                black_box("Processing file 1/100"),
                black_box("Processing file 2/100"),
                0.3,
            )
        });
    });

    group.finish();
}

// =============================================================================
// End-to-end: compress + stats + decompress
// =============================================================================

fn bench_full_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_full_pipeline");

    let input = generate_repetitive_output(1000);
    let config = CompressionConfig::default();

    group.throughput(Throughput::Bytes(input.len() as u64));
    group.bench_function("compress_stats_decompress", |b| {
        b.iter(|| {
            let compressed = compress(black_box(&input), &config);
            let _stats = compression_stats(&input, &compressed);
            let decompressed = decompress(&compressed);
            black_box(decompressed);
        });
    });

    group.finish();
}

// =============================================================================
// Helpers
// =============================================================================

fn generate_repetitive_output(line_count: usize) -> String {
    let patterns = [
        "Processing file {}/100",
        "Compiling crate {}/50 (frankenterm-core)",
        "test module::test_{} ... ok",
        "warning: unused variable `var_{}` at line {}",
    ];

    let mut lines = Vec::with_capacity(line_count);
    for i in 0..line_count {
        let pattern = patterns[i % patterns.len()];
        let line = pattern.replacen("{}", &(i + 1).to_string(), 1).replacen(
            "{}",
            &((i % 500) + 1).to_string(),
            1,
        );
        lines.push(line);
    }
    lines.join("\n")
}

fn generate_mixed_agent_output(line_count: usize) -> String {
    let unique_lines = [
        "error[E0308]: mismatched types",
        "  --> src/main.rs:42:9",
        "  |",
        "42 |     let x: u32 = \"hello\";",
        "  |                  ^^^^^^^ expected `u32`, found `&str`",
        "$ git status",
        "On branch main",
        "Your branch is up to date with 'origin/main'.",
        "Changes not staged for commit:",
    ];

    let repetitive = "Processing file {}/100";

    let mut lines = Vec::with_capacity(line_count);
    for i in 0..line_count {
        if i % 5 == 0 {
            // Unique line every 5th
            lines.push(unique_lines[i % unique_lines.len()].to_string());
        } else {
            lines.push(repetitive.replacen("{}", &(i + 1).to_string(), 1));
        }
    }
    lines.join("\n")
}

// =============================================================================
// Criterion groups and main
// =============================================================================

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("semantic_compression", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_compression_ratio_progress,
        bench_compression_ratio_mixed,
        bench_compression_throughput,
        bench_decompression_throughput,
        bench_template_extraction,
        bench_edit_distance,
        bench_full_pipeline
);
criterion_main!(benches);
