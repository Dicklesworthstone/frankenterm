//! Benchmarks for scrollback eviction under memory pressure.
//!
//! Bead: wa-3r5e
//!
//! Performance budgets:
//! - Eviction decision (50 panes): **< 1ms**
//! - Per-pane segment trim (10K→limit): **< 50us**
//! - Plan computation (100 panes): **< 2ms**
//! - Config max_segments_for lookup: **< 50ns**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::collections::HashMap;

use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::pane_tiers::PaneTier;
use frankenterm_core::scrollback_eviction::{
    EvictionConfig, PaneTierSource, ScrollbackEvictor, SegmentStore,
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
        group.bench_with_input(
            BenchmarkId::from_parameter(n_panes),
            &n_panes,
            |b, &n| {
                let evictor = ScrollbackEvictor::new(
                    EvictionConfig::default(),
                    BenchStore::mixed(n),
                    BenchTierSource::cyclic(n),
                );
                b.iter(|| evictor.plan(MemoryPressureTier::Yellow).unwrap());
            },
        );
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
        bench_pressure_escalation
);
criterion_main!(benches);
