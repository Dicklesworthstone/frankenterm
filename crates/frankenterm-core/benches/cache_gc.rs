//! Benchmarks for periodic cache GC maintenance.
//!
//! Targets:
//! - End-to-end GC cycle time for 50 caches with mixed churn
//! - Per-cache `shrink_to_fit` overhead with 10k entries / 80% dead
//! - SQLite VACUUM decision overhead (`PRAGMA page_count` + `freelist_count`)
//! - JSON GC report generation overhead for 50 caches

use std::collections::{HashMap, HashSet};
use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use frankenterm_core::gc::{CacheCompactionStats, compact_u64_map, should_vacuum};
use rusqlite::Connection;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "cache_gc/gc_cycle_time",
        budget: "<100ms for 50-cache mixed workload",
    },
    bench_common::BenchBudget {
        name: "cache_gc/shrink_to_fit_overhead",
        budget: "<5ms per cache (10k entries, 80% dead)",
    },
    bench_common::BenchBudget {
        name: "cache_gc/sqlite_vacuum_decision",
        budget: "<1ms for page_count + freelist_count + decision",
    },
    bench_common::BenchBudget {
        name: "cache_gc/gc_report_generation",
        budget: "<100us for 50-cache report build",
    },
];

fn build_cache(entries: usize, dead_percent: usize) -> (HashMap<u64, u64>, HashSet<u64>) {
    let dead_entries = entries.saturating_mul(dead_percent) / 100;
    let live_entries = entries.saturating_sub(dead_entries);

    let mut map = HashMap::with_capacity(entries.saturating_mul(2));
    for idx in 0..entries {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let key = idx as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let value = idx as u64;
        map.insert(key, value);
    }

    let active_keys: HashSet<u64> = (0..live_entries)
        .map(|idx| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                idx as u64
            }
        })
        .collect();

    (map, active_keys)
}

fn bench_gc_cycle_time(c: &mut Criterion) {
    c.bench_function("cache_gc/gc_cycle_time", |b| {
        b.iter_batched(
            || {
                let mut caches = Vec::with_capacity(50);
                let mut active_sets = Vec::with_capacity(50);
                for idx in 0..50 {
                    let dead_percent = if idx % 2 == 0 { 80 } else { 35 };
                    let (cache, active) = build_cache(10_000, dead_percent);
                    caches.push(cache);
                    active_sets.push(active);
                }
                (caches, active_sets)
            },
            |(mut caches, active_sets)| {
                let mut removed_total = 0usize;
                let mut freed_slots_total = 0usize;
                for (cache, active) in caches.iter_mut().zip(&active_sets) {
                    let stats = compact_u64_map(cache, active);
                    removed_total = removed_total.saturating_add(stats.removed_entries);
                    freed_slots_total = freed_slots_total.saturating_add(stats.freed_slots());
                }
                black_box((removed_total, freed_slots_total));
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_shrink_to_fit_overhead(c: &mut Criterion) {
    c.bench_function("cache_gc/shrink_to_fit_overhead", |b| {
        b.iter_batched(
            || build_cache(10_000, 80),
            |(mut cache, active)| {
                let stats = compact_u64_map(&mut cache, &active);
                black_box(stats);
            },
            BatchSize::SmallInput,
        );
    });
}

fn build_sqlite_fixture() -> Connection {
    let conn = Connection::open_in_memory().expect("open sqlite in-memory db");
    conn.execute_batch(
        "
        PRAGMA auto_vacuum = NONE;
        CREATE TABLE gc_bench (id INTEGER PRIMARY KEY, payload TEXT);
        ",
    )
    .expect("create sqlite fixture table");

    for idx in 0..20_000 {
        conn.execute(
            "INSERT INTO gc_bench (id, payload) VALUES (?1, ?2)",
            rusqlite::params![idx, "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"],
        )
        .expect("insert fixture row");
    }
    conn.execute("DELETE FROM gc_bench WHERE id % 3 = 0", [])
        .expect("delete fixture rows");

    conn
}

fn bench_sqlite_vacuum_decision(c: &mut Criterion) {
    let conn = build_sqlite_fixture();

    c.bench_function("cache_gc/sqlite_vacuum_decision", |b| {
        b.iter(|| {
            let page_count: i64 = conn
                .query_row("PRAGMA page_count", [], |row| row.get(0))
                .expect("query page_count");
            let free_pages: i64 = conn
                .query_row("PRAGMA freelist_count", [], |row| row.get(0))
                .expect("query freelist_count");
            black_box(should_vacuum(page_count, free_pages, 0.20));
        });
    });
}

fn bench_gc_report_generation(c: &mut Criterion) {
    let per_cache_stats: Vec<CacheCompactionStats> = (0..50usize)
        .map(|idx| CacheCompactionStats {
            before_len: 10_000,
            before_capacity: 16_384,
            after_len: 10_000usize.saturating_sub((idx % 8).saturating_mul(200)),
            after_capacity: 12_288,
            removed_entries: (idx % 8).saturating_mul(200),
        })
        .collect();

    c.bench_function("cache_gc/gc_report_generation", |b| {
        b.iter(|| {
            let caches: Vec<_> = per_cache_stats
                .iter()
                .enumerate()
                .map(|(idx, stats)| {
                    serde_json::json!({
                        "cache": format!("cache_{idx}"),
                        "before_len": stats.before_len,
                        "before_capacity": stats.before_capacity,
                        "after_len": stats.after_len,
                        "after_capacity": stats.after_capacity,
                        "removed_entries": stats.removed_entries,
                        "freed_slots": stats.freed_slots(),
                    })
                })
                .collect();

            let report = serde_json::json!({
                "active_panes": 50,
                "vacuum_threshold": 0.20,
                "free_ratio": 0.11,
                "vacuumed": false,
                "caches": caches,
            });
            black_box(report.to_string());
        });
    });
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("cache_gc", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_gc_cycle_time,
        bench_shrink_to_fit_overhead,
        bench_sqlite_vacuum_decision,
        bench_gc_report_generation
);
criterion_main!(benches);
