//! Benchmarks for scrollback eviction under memory pressure.
//!
//! Bead: wa-3r5e
//!
//! Performance budgets:
//! - Eviction decision (50 panes): **< 1ms**
//! - Per-pane segment trim (10K→limit): **< 50us**
//! - Plan computation (100 panes): **< 2ms**
//! - Config max_segments_for lookup: **< 50ns**

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::collections::{HashMap, VecDeque};
use std::hint::black_box;

use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::pane_tiers::PaneTier;
use frankenterm_core::scrollback_eviction::{
    EvictionConfig, ImportanceRetentionConfig, LineImportanceScorer, PaneTierSource,
    ScrollbackEvictor, ScrollbackLine, SegmentStore, enforce_importance_budget,
    select_importance_eviction_index, total_line_bytes,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "eviction_decision_50",
        budget: "p50 < 1ms (plan for 50 panes)",
    },
    bench_common::BenchBudget {
        name: "per_pane_trim",
        budget: "p50 < 50us (trim one pane)",
    },
    bench_common::BenchBudget {
        name: "plan_100_panes",
        budget: "p50 < 2ms (plan for 100 panes)",
    },
    bench_common::BenchBudget {
        name: "config_lookup",
        budget: "p50 < 50ns (max_segments_for)",
    },
    bench_common::BenchBudget {
        name: "score_line_latency",
        budget: "p50 < 1us (importance score for one line)",
    },
    bench_common::BenchBudget {
        name: "score_batch_throughput",
        budget: "> 1M lines/sec (10k mixed-content batch)",
    },
    bench_common::BenchBudget {
        name: "eviction_selection",
        budget: "p50 < 50us (select victim from 10k lines, oldest 25%)",
    },
    bench_common::BenchBudget {
        name: "byte_budget_enforcement",
        budget: "< 5% overhead vs line-count-only trim baseline",
    },
];

// ── Mock implementations for benchmarks ──────────────────────────────

#[derive(Debug)]
struct BenchStore {
    segments: HashMap<u64, usize>,
}

impl BenchStore {
    fn uniform(n: usize, segments_per_pane: usize) -> Self {
        Self {
            segments: (0..n as u64).map(|id| (id, segments_per_pane)).collect(),
        }
    }

    fn mixed(n: usize) -> Self {
        let tiers = [10_000, 5_000, 1_500, 800, 300];
        Self {
            segments: (0..n as u64)
                .map(|id| (id, tiers[id as usize % tiers.len()]))
                .collect(),
        }
    }
}

impl SegmentStore for BenchStore {
    fn count_segments(&self, pane_id: u64) -> Result<usize, String> {
        Ok(*self.segments.get(&pane_id).unwrap_or(&0))
    }

    fn delete_oldest_segments(&self, _pane_id: u64, count: usize) -> Result<usize, String> {
        Ok(count)
    }

    fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
        let mut ids: Vec<_> = self.segments.keys().copied().collect();
        ids.sort();
        Ok(ids)
    }
}

struct BenchTierSource {
    tiers: HashMap<u64, PaneTier>,
}

impl BenchTierSource {
    fn cyclic(n: usize) -> Self {
        let all_tiers = [
            PaneTier::Active,
            PaneTier::Thinking,
            PaneTier::Idle,
            PaneTier::Background,
            PaneTier::Dormant,
        ];
        Self {
            tiers: (0..n as u64)
                .map(|id| (id, all_tiers[id as usize % all_tiers.len()]))
                .collect(),
        }
    }
}

impl PaneTierSource for BenchTierSource {
    fn tier_for(&self, pane_id: u64) -> Option<PaneTier> {
        self.tiers.get(&pane_id).copied()
    }
}

// ── Benchmark: config max_segments_for lookup ────────────────────────

fn bench_config_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("scrollback_eviction/config_lookup");

    let config = EvictionConfig::default();
    let tiers = [
        PaneTier::Active,
        PaneTier::Thinking,
        PaneTier::Idle,
        PaneTier::Background,
        PaneTier::Dormant,
    ];
    let pressures = [
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ];

    group.bench_function("all_combos", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for &tier in &tiers {
                for &pressure in &pressures {
                    total += config.max_segments_for(tier, pressure);
                }
            }
            total
        });
    });

    group.finish();
}

// ── Benchmark: plan computation at various scales ────────────────────

fn bench_plan_computation(c: &mut Criterion) {
    let mut group = c.benchmark_group("scrollback_eviction/plan");

    for &n_panes in &[10, 50, 100, 200] {
        group.throughput(Throughput::Elements(n_panes as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n_panes), &n_panes, |b, &n| {
            let evictor = ScrollbackEvictor::new(
                EvictionConfig::default(),
                BenchStore::mixed(n),
                BenchTierSource::cyclic(n),
            );
            b.iter(|| evictor.plan(MemoryPressureTier::Yellow).unwrap());
        });
    }

    group.finish();
}

// ── Benchmark: plan + execute (full eviction cycle) ──────────────────

fn bench_evict_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("scrollback_eviction/evict");

    for &pressure in &[
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ] {
        let label = format!("{pressure:?}");
        group.bench_function(BenchmarkId::new("50_panes", &label), |b| {
            let evictor = ScrollbackEvictor::new(
                EvictionConfig::default(),
                BenchStore::mixed(50),
                BenchTierSource::cyclic(50),
            );
            b.iter(|| evictor.evict(pressure).unwrap());
        });
    }

    group.finish();
}

// ── Benchmark: per-pane trim (single pane, high segment count) ───────

fn bench_per_pane_trim(c: &mut Criterion) {
    let mut group = c.benchmark_group("scrollback_eviction/per_pane_trim");

    for &segment_count in &[1_000, 10_000, 50_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(segment_count),
            &segment_count,
            |b, &count| {
                let evictor = ScrollbackEvictor::new(
                    EvictionConfig::default(),
                    BenchStore::uniform(1, count),
                    BenchTierSource::cyclic(1),
                );
                // Pane 0 is Active (from cyclic), Red pressure forces limit to 200
                b.iter(|| evictor.evict(MemoryPressureTier::Red).unwrap());
            },
        );
    }

    group.finish();
}

// ── Benchmark: pressure escalation (same panes, increasing pressure) ─

fn bench_pressure_escalation(c: &mut Criterion) {
    let mut group = c.benchmark_group("scrollback_eviction/pressure_escalation");

    let evictor = ScrollbackEvictor::new(
        EvictionConfig::default(),
        BenchStore::uniform(50, 5_000),
        BenchTierSource::cyclic(50),
    );

    for &pressure in &[
        MemoryPressureTier::Green,
        MemoryPressureTier::Yellow,
        MemoryPressureTier::Orange,
        MemoryPressureTier::Red,
    ] {
        let label = format!("{pressure:?}");
        group.bench_function(&label, |b| {
            b.iter(|| {
                let plan = evictor.plan(pressure).unwrap();
                plan.total_segments_to_remove
            });
        });
    }

    group.finish();
}

fn synthesize_scored_lines(n: usize) -> VecDeque<ScrollbackLine> {
    let mut lines = VecDeque::with_capacity(n);
    for i in 0..n {
        let text = match i % 7 {
            0 => format!("error: compilation failed in crate module_{i}"),
            1 => format!("[#####-----] {}% complete", i % 100),
            2 => format!("Using tool: Bash (step {i})"),
            3 => format!("test result: {} passed; {} failed", i % 20, i % 3),
            4 => format!("Compiling crate_{i} v0.1.0"),
            5 => "\u{1b}[2K\u{1b}[1A".to_string(),
            _ => format!("regular output line {i}"),
        };
        let importance = match i % 6 {
            0 => 0.95,
            1 => 0.10,
            2 => 0.85,
            3 => 0.75,
            4 => 0.55,
            _ => 0.30,
        };
        lines.push_back(ScrollbackLine::new(text, importance, i as u64));
    }
    lines
}

// ── Benchmarks: wa-1c2u importance-weighted retention ────────────────

fn bench_importance_score_line_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("scrollback_eviction/importance_score_line_latency");
    let scorer = LineImportanceScorer::default();
    let line = "error: failed to compile crate foo; Using tool: Bash";

    group.bench_function("score_line_latency", |b| {
        b.iter(|| scorer.score_line(black_box(line), None));
    });
    group.finish();
}

fn bench_importance_score_batch_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("scrollback_eviction/importance_score_batch_throughput");
    let scorer = LineImportanceScorer::default();
    let lines: Vec<String> = (0..10_000)
        .map(|i| match i % 6 {
            0 => format!("error: compile failed for module_{i}"),
            1 => format!("Compiling crate_{i}"),
            2 => format!("Using tool: Bash #{i}"),
            3 => format!("test result: {} passed; 0 failed", i % 50),
            4 => format!("[######----] {}% complete", i % 100),
            _ => format!("normal output line {i}"),
        })
        .collect();

    group.throughput(Throughput::Elements(lines.len() as u64));
    group.bench_function("score_batch_throughput", |b| {
        b.iter(|| {
            let mut prev: Option<&str> = None;
            let mut total = 0.0;
            for line in &lines {
                total += scorer.score_line(line, prev);
                prev = Some(line);
            }
            black_box(total);
        });
    });
    group.finish();
}

fn bench_importance_eviction_selection(c: &mut Criterion) {
    let mut group = c.benchmark_group("scrollback_eviction/importance_eviction_selection");
    let lines = synthesize_scored_lines(10_000);
    let config = ImportanceRetentionConfig {
        min_lines: 1,
        max_lines: 10_000,
        oldest_window_fraction: 0.25,
        ..Default::default()
    };

    group.bench_function("eviction_selection", |b| {
        b.iter(|| select_importance_eviction_index(black_box(&lines), black_box(&config)));
    });
    group.finish();
}

fn bench_importance_budget_enforcement(c: &mut Criterion) {
    let mut group = c.benchmark_group("scrollback_eviction/importance_byte_budget_enforcement");
    let seed = synthesize_scored_lines(10_000);
    let total = total_line_bytes(&seed);
    let config = ImportanceRetentionConfig {
        byte_budget_per_pane: total / 2,
        min_lines: 500,
        max_lines: 10_000,
        importance_threshold: 0.8,
        oldest_window_fraction: 0.25,
    };
    let baseline_target_len = seed.len() / 2;

    group.bench_function("line_count_only_trim_baseline", |b| {
        b.iter_batched(
            || seed.clone(),
            |mut lines| {
                while lines.len() > baseline_target_len && lines.len() > config.min_lines {
                    let _ = lines.pop_front();
                }
                black_box(lines.len());
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("byte_budget_enforcement", |b| {
        b.iter_batched(
            || seed.clone(),
            |mut lines| {
                let report = enforce_importance_budget(&mut lines, &config);
                black_box(report.remaining_lines);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("scrollback_eviction", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_config_lookup,
        bench_plan_computation,
        bench_evict_cycle,
        bench_per_pane_trim,
        bench_pressure_escalation,
        bench_importance_score_line_latency,
        bench_importance_score_batch_throughput,
        bench_importance_eviction_selection,
        bench_importance_budget_enforcement
);
criterion_main!(benches);
