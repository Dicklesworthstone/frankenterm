//! Benchmarks for OSC 133 marker parsing.
//!
//! This path runs on captured snapshots when shell-state inference is enabled.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::ingest::parse_osc133_markers;
use std::fmt::Write;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[bench_common::BenchBudget {
    name: "osc133_markers",
    budget: "p50 < 10µs for large sparse-marker snapshots",
}];

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

fn with_sparse_markers(lines: usize) -> String {
    format!(
        "{}\x1b]133;A\x07prompt\x1b]133;B\x07{}\x1b]133;D;0\x07{}",
        generate_content(lines / 2),
        generate_content(8),
        generate_content(lines / 2)
    )
}

fn bench_plain_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("osc133_markers/plain_snapshot");
    for lines in [1000usize, 4000] {
        let text = generate_content(lines);
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(lines), &text, |b, text| {
            b.iter(|| parse_osc133_markers(text))
        });
    }
    group.finish();
}

fn bench_sparse_markers(c: &mut Criterion) {
    let mut group = c.benchmark_group("osc133_markers/sparse_markers");
    for lines in [1000usize, 4000] {
        let text = with_sparse_markers(lines);
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(lines), &text, |b, text| {
            b.iter(|| parse_osc133_markers(text))
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = bench_common::default_criterion();
    targets = bench_plain_snapshot, bench_sparse_markers
}
criterion_main!(benches);

#[allow(clippy::used_underscore_items)]
fn _emit_bench_artifacts() {
    bench_common::emit_bench_artifacts("osc133_markers", BUDGETS);
}
