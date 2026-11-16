//! Round-6 M4 bench: adaptive CDC auto-enable for redundant scrollback pages.
//!
//! The benchmark constructs through `TieredScrollback::new` so
//! `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP` is the runtime A/B gate. Default is the
//! legacy standalone-zstd warm page; setting the env var to `adaptive` enables
//! the cheap redundancy probe, while truthy values force always-on CDC.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::scrollback_tiers::{ScrollbackConfig, TieredScrollback};

mod bench_common;

const LINE_COUNT: usize = 2_048;
const HOT_LINES: usize = 32;
const PAGE_SIZE: usize = 32;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "scrollback_cdc_adaptive/low_redundancy_build",
        budget: "A/B env gate FT_MOONSHOT_SCROLLBACK_CDC_DEDUP=adaptive should stay close to env-OFF when the cheap probe rejects low-redundancy pages",
    },
    bench_common::BenchBudget {
        name: "scrollback_cdc_adaptive/redundant_build",
        budget: "A/B env gate FT_MOONSHOT_SCROLLBACK_CDC_DEDUP=adaptive should recover CDC warm-byte savings on redundant scrollback without always-on cost for non-redundant panes",
    },
];

fn scrollback_config() -> ScrollbackConfig {
    ScrollbackConfig {
        hot_lines: HOT_LINES,
        page_size: PAGE_SIZE,
        warm_max_bytes: 128 * 1024 * 1024,
        compression: Default::default(),
        cold_eviction_enabled: false,
    }
}

fn low_redundancy_line(i: usize) -> String {
    let mut x = (i as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0xD1B5_4A32_D192_ED03);
    let mut out = format!("unique-scrollback-bench-line-{i:05}:");
    for _ in 0..48 {
        out.push(' ');
        out.push_str(&format!("{x:016x}"));
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    out
}

fn redundant_line(i: usize) -> String {
    format!(
        "pane=042 redraw-slot={} :: {} :: {}",
        i % 8,
        "STABLE-TERMINAL-REDRAW ".repeat(24),
        "same-prompt-and-output-bytes ".repeat(16)
    )
}

fn build_scrollback(lines: &[String]) -> TieredScrollback {
    let mut scrollback = TieredScrollback::new(scrollback_config());
    scrollback.push_lines(lines.iter().cloned());
    scrollback
}

fn bench_cdc_adaptive(c: &mut Criterion) {
    let low_redundancy: Vec<String> = (0..LINE_COUNT).map(low_redundancy_line).collect();
    let redundant: Vec<String> = (0..LINE_COUNT).map(redundant_line).collect();
    let mut group = c.benchmark_group("scrollback_cdc_adaptive");

    group.bench_function("low_redundancy_build", |b| {
        b.iter(|| {
            let scrollback = build_scrollback(black_box(&low_redundancy));
            black_box((
                scrollback.warm_total_bytes(),
                scrollback.cdc_stats(),
                scrollback.cdc_adaptive_snapshot(),
            ));
        });
    });

    group.bench_function("redundant_build", |b| {
        b.iter(|| {
            let scrollback = build_scrollback(black_box(&redundant));
            black_box((
                scrollback.warm_total_bytes(),
                scrollback.cdc_stats(),
                scrollback.cdc_adaptive_snapshot(),
            ));
        });
    });

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_cdc_adaptive(c);
    bench_common::emit_bench_artifacts("scrollback_cdc_adaptive", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
