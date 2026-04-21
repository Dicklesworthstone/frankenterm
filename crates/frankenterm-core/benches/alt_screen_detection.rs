//! Benchmarks for alternate-screen transition detection.
//!
//! This path runs on every captured snapshot before delta extraction.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::ingest::{detect_alt_screen_changes, has_alt_screen_change};
use std::fmt::Write;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[bench_common::BenchBudget {
    name: "alt_screen_detection",
    budget: "p50 < 200µs for large no-escape snapshots",
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

fn with_alt_screen_toggles(before_lines: usize, middle_lines: usize, after_lines: usize) -> String {
    format!(
        "{}\x1b[?1049h{}\x1b[?1049l{}",
        generate_content(before_lines),
        generate_content(middle_lines),
        generate_content(after_lines)
    )
}

fn bench_detect_no_escape(c: &mut Criterion) {
    let mut group = c.benchmark_group("alt_screen_detection/detect_no_escape");
    for lines in [100, 1000, 2000] {
        let text = generate_content(lines);
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_with_input(BenchmarkId::new("lines", lines), &text, |b, text| {
            b.iter(|| detect_alt_screen_changes(text))
        });
    }
    group.finish();
}

fn bench_detect_with_toggles(c: &mut Criterion) {
    let mut group = c.benchmark_group("alt_screen_detection/detect_with_toggles");
    for middle_lines in [50, 200, 500] {
        let text = with_alt_screen_toggles(1000, middle_lines, 1000);
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("middle_lines", middle_lines),
            &text,
            |b, text| b.iter(|| detect_alt_screen_changes(text)),
        );
    }
    group.finish();
}

fn bench_has_change(c: &mut Criterion) {
    let mut group = c.benchmark_group("alt_screen_detection/has_change");
    let no_escape = generate_content(2000);
    let with_toggle = with_alt_screen_toggles(1000, 200, 1000);

    for (label, text) in [("no_escape", &no_escape), ("with_toggle", &with_toggle)] {
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_with_input(BenchmarkId::new("case", label), text, |b, text| {
            b.iter(|| has_alt_screen_change(text))
        });
    }
    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("alt_screen_detection", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_detect_no_escape, bench_detect_with_toggles, bench_has_change
);
criterion_main!(benches);
