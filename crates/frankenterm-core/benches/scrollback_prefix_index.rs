//! Round-5 Q1 bench: deep scrollback offset lookup with the prefix-index gate.
//!
//! The benchmark intentionally constructs through `TieredScrollback::new` so
//! `FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX` is the runtime A/B gate. Default is
//! the legacy linear walk; setting the env var enables the prefix index.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::scrollback_tiers::{ScrollbackConfig, TieredScrollback};

mod bench_common;

const LINE_COUNT: usize = 48_000;
const SHALLOW_LINE_COUNT: usize = 4_096;
const HOT_LINES: usize = 128;
const SHALLOW_HOT_LINES: usize = 8_192;
const PAGE_SIZE: usize = 64;
const OFFSET_PROBES: usize = 8_192;
const SCROLLBACK_LINE: &str = "pane-042 deterministic round5 deep-scroll payload";

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "scrollback_prefix_index/deep_scroll_locate_offset",
        budget: "A/B env gate FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX should make deep locate_offset/tier_for_offset scale with binary-search prefix lookup instead of page scans",
    },
    bench_common::BenchBudget {
        name: "scrollback_prefix_index/shallow_hot_locate_offset",
        budget: "A/B env gate FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX should be approximately env-OFF for hot-tier-only shallow scrollback",
    },
];

fn scrollback_config() -> ScrollbackConfig {
    ScrollbackConfig {
        hot_lines: HOT_LINES,
        page_size: PAGE_SIZE,
        warm_max_bytes: 256 * 1024 * 1024,
        compression: Default::default(),
        cold_eviction_enabled: false,
    }
}

fn build_deep_scrollback() -> TieredScrollback {
    let mut scrollback = TieredScrollback::new(scrollback_config());
    scrollback.push_lines(std::iter::repeat_with(|| SCROLLBACK_LINE.to_owned()).take(LINE_COUNT));
    scrollback
}

fn build_shallow_scrollback() -> TieredScrollback {
    let mut scrollback = TieredScrollback::new(ScrollbackConfig {
        hot_lines: SHALLOW_HOT_LINES,
        page_size: PAGE_SIZE,
        warm_max_bytes: 256 * 1024 * 1024,
        compression: Default::default(),
        cold_eviction_enabled: false,
    });
    scrollback
        .push_lines(std::iter::repeat_with(|| SCROLLBACK_LINE.to_owned()).take(SHALLOW_LINE_COUNT));
    assert_eq!(scrollback.warm_page_count(), 0);
    assert_eq!(scrollback.cold_page_count(), 0);
    scrollback
}

fn deep_offsets(scrollback: &TieredScrollback) -> Vec<usize> {
    let total = usize::try_from(scrollback.total_line_count()).unwrap_or(usize::MAX);
    let warm_span = total.saturating_sub(scrollback.hot_len()).max(1);
    (0..OFFSET_PROBES)
        .map(|idx| scrollback.hot_len() + (idx.wrapping_mul(97).wrapping_add(31) % warm_span))
        .collect()
}

fn shallow_offsets(scrollback: &TieredScrollback) -> Vec<usize> {
    let hot_span = scrollback.hot_len().max(1);
    (0..OFFSET_PROBES)
        .map(|idx| idx.wrapping_mul(31).wrapping_add(7) % hot_span)
        .collect()
}

fn bench_deep_locate_offset(c: &mut Criterion) {
    let scrollback = build_deep_scrollback();
    let offsets = deep_offsets(&scrollback);
    let mut group = c.benchmark_group("scrollback_prefix_index");

    group.bench_function("deep_scroll_locate_offset", |b| {
        b.iter(|| {
            let mut warm = 0usize;
            let mut cold = 0usize;
            for &offset in &offsets {
                match scrollback.locate_offset(black_box(offset)) {
                    Some(frankenterm_core::scrollback_tiers::ScrollbackLocationHint::Warm {
                        ..
                    }) => {
                        warm = warm.saturating_add(1);
                    }
                    Some(frankenterm_core::scrollback_tiers::ScrollbackLocationHint::Cold {
                        ..
                    }) => {
                        cold = cold.saturating_add(1);
                    }
                    _ => {}
                }
            }
            black_box((warm, cold, scrollback.prefix_index_active()));
        });
    });

    group.bench_function("deep_scroll_tier_for_offset", |b| {
        b.iter(|| {
            let mut warm = 0usize;
            for &offset in &offsets {
                if matches!(
                    scrollback.tier_for_offset(black_box(offset)),
                    frankenterm_core::scrollback_tiers::ScrollbackTier::Warm
                ) {
                    warm = warm.saturating_add(1);
                }
            }
            black_box((warm, scrollback.prefix_index_active()));
        });
    });

    group.finish();
}

fn bench_shallow_hot(c: &mut Criterion) {
    let scrollback = build_shallow_scrollback();
    let offsets = shallow_offsets(&scrollback);
    let mut group = c.benchmark_group("scrollback_prefix_index");

    group.bench_function("shallow_hot_locate_offset", |b| {
        b.iter(|| {
            let mut hot = 0usize;
            for &offset in &offsets {
                if matches!(
                    scrollback.locate_offset(black_box(offset)),
                    Some(frankenterm_core::scrollback_tiers::ScrollbackLocationHint::Hot { .. })
                ) {
                    hot = hot.saturating_add(1);
                }
            }
            black_box((hot, scrollback.prefix_index_active()));
        });
    });

    group.bench_function("shallow_hot_tier_for_offset", |b| {
        b.iter(|| {
            let mut hot = 0usize;
            for &offset in &offsets {
                if matches!(
                    scrollback.tier_for_offset(black_box(offset)),
                    frankenterm_core::scrollback_tiers::ScrollbackTier::Hot
                ) {
                    hot = hot.saturating_add(1);
                }
            }
            black_box((hot, scrollback.prefix_index_active()));
        });
    });

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_deep_locate_offset(c);
    bench_shallow_hot(c);
    bench_common::emit_bench_artifacts("scrollback_prefix_index", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
