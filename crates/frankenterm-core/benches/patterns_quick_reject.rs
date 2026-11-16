//! Criterion benchmarks for `PatternEngine::quick_reject()`.
//!
//! This isolates the cheap public prefilter path from the heavier `detect()`
//! flow so we can track regressions in the memchr + Bloom gate directly.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::patterns::PatternEngine;
use std::hint::black_box;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[bench_common::BenchBudget {
    name: "patterns_quick_reject",
    budget: "median < 5ms across no-match and near-miss corpora",
}];

const SIZES: &[usize] = &[1024, 16_384, 65_536, 1_048_576];

fn repeat_to_size(lines: &[&str], size: usize) -> String {
    let mut text = String::with_capacity(size);
    let mut i = 0;
    while text.len() < size {
        text.push_str(lines[i % lines.len()]);
        i += 1;
    }
    text.truncate(size);
    text
}

fn no_match_payload(size: usize) -> String {
    repeat_to_size(
        &[
            "2026-04-21T00:00:00Z INFO worker heartbeat steady\n",
            "pane status idle; scheduler healthy; backoff disabled\n",
            "letters and digits only abcdefghijklmnopqrstuvwxyz 0123456789\n",
        ],
        size,
    )
}

fn near_miss_payload(size: usize) -> String {
    repeat_to_size(
        &[
            "codex resumed last session successfully without warning banner\n",
            "usage metrics collected for dashboard export only\n",
            "rate limiter sleeping disabled; retry window unavailable\n",
            "Claude session summary omitted compacted token details\n",
        ],
        size,
    )
}

fn anchored_match_payload(size: usize) -> String {
    repeat_to_size(
        &[
            "Warning: less than 10% of your 20h limit remaining.\n",
            "To check your remaining time, run: codex usage\n",
            "Status line between repeated warnings to emulate scrollback.\n",
        ],
        size,
    )
}

fn bench_quick_reject_no_match(c: &mut Criterion) {
    let engine = PatternEngine::new();
    let _ = engine.quick_reject("warmup");

    let mut group = c.benchmark_group("patterns_quick_reject/no_match_sizes");
    for &size in SIZES {
        let payload = no_match_payload(size);
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size}B")),
            &payload,
            |b, text| {
                b.iter(|| engine.quick_reject(black_box(text)));
            },
        );
    }
    group.finish();
}

fn bench_quick_reject_near_miss(c: &mut Criterion) {
    let engine = PatternEngine::new();
    let _ = engine.quick_reject("warmup");

    let mut group = c.benchmark_group("patterns_quick_reject/near_miss_sizes");
    for &size in SIZES {
        let payload = near_miss_payload(size);
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size}B")),
            &payload,
            |b, text| {
                b.iter(|| engine.quick_reject(black_box(text)));
            },
        );
    }
    group.finish();
}

fn bench_quick_reject_anchor_hit(c: &mut Criterion) {
    let engine = PatternEngine::new();
    let _ = engine.quick_reject("warmup");

    let mut group = c.benchmark_group("patterns_quick_reject/anchor_hit_sizes");
    for &size in SIZES {
        let payload = anchored_match_payload(size);
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size}B")),
            &payload,
            |b, text| {
                b.iter(|| engine.quick_reject(black_box(text)));
            },
        );
    }
    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("patterns_quick_reject", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_quick_reject_no_match,
        bench_quick_reject_near_miss,
        bench_quick_reject_anchor_hit
);
criterion_main!(benches);
