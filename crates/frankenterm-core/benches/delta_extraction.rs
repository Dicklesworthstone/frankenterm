//! Benchmarks for delta extraction (overlap matching).
//!
//! This is the hot ingest path - every capture runs through delta extraction.
//!
//! Performance budgets:
//! - Delta extraction should complete in microseconds, not milliseconds
//! - Should scale reasonably with content size up to typical pane buffers (~100KB)

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::ingest::extract_delta;
use std::fmt::Write;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[bench_common::BenchBudget {
    name: "delta_extraction",
    budget: "p50 < 200µs, p99 < 1ms (typical overlap sizes)",
}];

/// Default overlap window size from RuntimeConfig.
const DEFAULT_OVERLAP_SIZE: usize = 4096;

/// Generate terminal-like content of specified approximate size.
fn generate_content(lines: usize) -> String {
    let mut content = String::with_capacity(lines * 80);
    for i in 0..lines {
        let _ = writeln!(
            &mut content,
            "[{}] Processing item {} - status: OK - elapsed: {}ms",
            i % 1000,
            i,
            (i * 7) % 100
        );
    }
    content
}

/// Scenario: Append-only (typical shell output).
/// Previous content is a prefix of current content.
fn bench_append_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_append_only");

    for lines in [10, 100, 500, 1000] {
        let prev = generate_content(lines);
        // Current is previous + 10 more lines
        let curr = format!("{}{}", prev, generate_content(10));

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("lines", lines),
            &(prev.clone(), curr.clone()),
            |b, (prev, curr)| b.iter(|| extract_delta(prev, curr, DEFAULT_OVERLAP_SIZE)),
        );
    }

    group.finish();
}

/// Scenario: No change (content identical).
fn bench_no_change(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_no_change");

    for lines in [10, 100, 500, 1000] {
        let content = generate_content(lines);

        group.throughput(Throughput::Bytes(content.len() as u64));
        group.bench_with_input(BenchmarkId::new("lines", lines), &content, |b, content| {
            b.iter(|| extract_delta(content, content, DEFAULT_OVERLAP_SIZE));
        });
    }

    group.finish();
}

/// Scenario: Small edit (middle of content changes).
/// This should fail overlap matching and produce a Gap.
fn bench_edit_middle(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_edit_middle");

    for lines in [100, 500, 1000] {
        let prev = generate_content(lines);
        // Modify a line in the middle
        let curr = prev.replacen("status: OK", "status: CHANGED", 1);

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("lines", lines),
            &(prev.clone(), curr.clone()),
            |b, (prev, curr)| b.iter(|| extract_delta(prev, curr, DEFAULT_OVERLAP_SIZE)),
        );
    }

    group.finish();
}

/// Scenario: Scrollback truncation (common in terminal buffers).
/// Previous content is longer, current content is a suffix.
fn bench_truncation(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_truncation");

    for prev_lines in [500, 1000, 2000] {
        let prev = generate_content(prev_lines);
        // Current keeps only last 100 lines (simulating scrollback)
        let lines: Vec<&str> = prev.lines().collect();
        let curr = lines[lines.len().saturating_sub(100)..].join("\n") + "\n";

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("prev_lines", prev_lines),
            &(prev.clone(), curr.clone()),
            |b, (prev, curr)| b.iter(|| extract_delta(prev, curr, DEFAULT_OVERLAP_SIZE)),
        );
    }

    group.finish();
}

/// Scenario: Varying overlap window sizes.
fn bench_overlap_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_overlap_sizes");

    let lines = 500;
    let prev = generate_content(lines);
    let curr = format!("{}{}", prev, generate_content(5));

    for overlap_size in [512, 1024, 2048, 4096, 8192] {
        group.bench_with_input(
            BenchmarkId::new("overlap_size", overlap_size),
            &(prev.clone(), curr.clone(), overlap_size),
            |b, (prev, curr, overlap)| b.iter(|| extract_delta(prev, curr, *overlap)),
        );
    }

    group.finish();
}

/// Scenario: First capture (empty previous).
fn bench_first_capture(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_first_capture");

    for lines in [10, 100, 500] {
        let curr = generate_content(lines);

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(BenchmarkId::new("lines", lines), &curr, |b, curr| {
            b.iter(|| extract_delta("", curr, DEFAULT_OVERLAP_SIZE));
        });
    }

    group.finish();
}

/// Scenario: Large content (stress test).
fn bench_large_content(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_large_content");
    group.sample_size(20); // Fewer samples for large content

    for lines in [5000, 10000] {
        let prev = generate_content(lines);
        let curr = format!("{}{}", prev, generate_content(10));

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("lines", lines),
            &(prev.clone(), curr.clone()),
            |b, (prev, curr)| b.iter(|| extract_delta(prev, curr, DEFAULT_OVERLAP_SIZE)),
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("delta_extraction", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_append_only,
        bench_no_change,
        bench_edit_middle,
        bench_truncation,
        bench_overlap_sizes,
        bench_first_capture,
        bench_large_content
);
criterion_main!(benches);
