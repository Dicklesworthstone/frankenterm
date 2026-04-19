//! Criterion benchmarks for O(1) LFU cache.
//!
//! Bead: ft-283h4.38
//!
//! Performance budgets:
//! - Single insert:    **< 100ns** (hash + bucket link)
//! - Get (hit):        **< 50ns** (hash + frequency bump)
//! - Peek:             **< 30ns** (hash lookup, no frequency bump)
//! - Remove:           **< 100ns** (hash + unlink)
//! - Insert w/evict:   **< 150ns** (insert + evict LFU entry)
//! - 10K inserts:      **< 1ms** total throughput

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use frankenterm_core::lfu_cache::{LfuCache, LfuCacheConfig, LfuCacheStats};

mod bench_common;

/// Performance budgets for CI regression detection.
#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "lfu_cache_insert",
        budget: "p50 < 100ns (hash + bucket link)",
    },
    bench_common::BenchBudget {
        name: "lfu_cache_get_hit",
        budget: "p50 < 50ns (hash + frequency bump)",
    },
    bench_common::BenchBudget {
        name: "lfu_cache_peek",
        budget: "p50 < 30ns (hash only, no frequency update)",
    },
    bench_common::BenchBudget {
        name: "lfu_cache_remove",
        budget: "p50 < 100ns (hash + unlink)",
    },
    bench_common::BenchBudget {
        name: "lfu_cache_insert_evict",
        budget: "p50 < 150ns (insert with LFU eviction)",
    },
    bench_common::BenchBudget {
        name: "lfu_cache_throughput",
        budget: "10K inserts < 1ms",
    },
];

// ── Data generators ────────────────────────────────────────────────────

/// Deterministic pseudo-random u64 sequence via LCG.
fn random_keys(n: usize) -> Vec<u64> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(state >> 32);
    }
    out
}

/// Zipf-like access pattern: most accesses hit a few hot keys.
fn zipf_accesses(n: usize, total_keys: u64) -> Vec<u64> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xCAFE_1234;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (state >> 33) as f64 / u32::MAX as f64;
        // Power-law: small keys much more likely
        let key = (r.powf(2.0) * total_keys as f64) as u64;
        out.push(key);
    }
    out
}

/// Pre-build a cache filled to capacity with sequential keys.
fn build_full_cache(capacity: usize) -> LfuCache<u64, u64> {
    let mut cache = LfuCache::new(capacity);
    for i in 0..capacity as u64 {
        cache.insert(i, i * 10);
    }
    cache
}

/// Pre-build a cache with a hot-cold access pattern.
fn build_hot_cold_cache(capacity: usize) -> LfuCache<u64, u64> {
    let mut cache = LfuCache::new(capacity);
    for i in 0..capacity as u64 {
        cache.insert(i, i * 10);
    }
    // Make first 10% "hot" (higher frequency)
    let hot_count = (capacity / 10).max(1);
    for _ in 0..5 {
        for i in 0..hot_count as u64 {
            cache.get(&i);
        }
    }
    cache
}

// ── Benchmarks ─────────────────────────────────────────────────────────

/// Benchmark: insert latency.
fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfu_cache/insert");

    // Cold insert (empty cache).
    group.bench_function("cold", |b| {
        b.iter_batched(
            || LfuCache::new(1000),
            |mut cache: LfuCache<u64, u64>| {
                cache.insert(black_box(42), black_box(100));
                cache
            },
            BatchSize::SmallInput,
        );
    });

    // Insert into half-full cache.
    group.bench_function("half_full_1000", |b| {
        b.iter_batched(
            || build_full_cache(500),
            |mut cache| {
                cache.insert(black_box(999), black_box(9990));
                cache
            },
            BatchSize::SmallInput,
        );
    });

    // Insert with eviction (cache at capacity).
    group.bench_function("eviction_1000", |b| {
        b.iter_batched(
            || build_full_cache(1000),
            |mut cache| {
                cache.insert(black_box(99999), black_box(99));
                cache
            },
            BatchSize::SmallInput,
        );
    });

    // Update existing key (no eviction, frequency bump).
    group.bench_function("update_existing", |b| {
        b.iter_batched(
            || build_full_cache(1000),
            |mut cache| {
                cache.insert(black_box(500), black_box(5555));
                cache
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: get (hit and miss) latency.
fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfu_cache/get");

    // Hit on existing key.
    group.bench_function("hit_1000", |b| {
        b.iter_batched(
            || build_full_cache(1000),
            |mut cache| {
                black_box(cache.get(black_box(&500)));
                cache
            },
            BatchSize::SmallInput,
        );
    });

    // Miss (key not present).
    group.bench_function("miss_1000", |b| {
        b.iter_batched(
            || build_full_cache(1000),
            |mut cache| {
                black_box(cache.get(black_box(&99999)));
                cache
            },
            BatchSize::SmallInput,
        );
    });

    // Hit on hot key (high frequency).
    group.bench_function("hit_hot_key", |b| {
        b.iter_batched(
            || build_hot_cold_cache(1000),
            |mut cache| {
                black_box(cache.get(black_box(&0)));
                cache
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: peek (no frequency bump).
fn bench_peek(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfu_cache/peek");

    group.bench_function("hit_1000", |b| {
        let cache = build_full_cache(1000);
        b.iter(|| black_box(cache.peek(black_box(&500))));
    });

    group.bench_function("miss_1000", |b| {
        let cache = build_full_cache(1000);
        b.iter(|| black_box(cache.peek(black_box(&99999))));
    });

    group.finish();
}

/// Benchmark: remove latency.
fn bench_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfu_cache/remove");

    group.bench_function("existing_1000", |b| {
        b.iter_batched(
            || build_full_cache(1000),
            |mut cache| {
                black_box(cache.remove(black_box(&500)));
                cache
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("nonexistent", |b| {
        b.iter_batched(
            || build_full_cache(1000),
            |mut cache| {
                black_box(cache.remove(black_box(&99999)));
                cache
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: insert+get throughput at different cache sizes.
fn bench_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfu_cache/throughput");

    // Insert throughput (sequential keys, no eviction).
    for count in [100, 1_000, 10_000] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(
            BenchmarkId::new("insert_no_evict", count),
            &count,
            |b, &count| {
                b.iter(|| {
                    let mut cache: LfuCache<u64, u64> = LfuCache::new(count as usize + 1);
                    for i in 0..count {
                        cache.insert(black_box(i), black_box(i * 10));
                    }
                    black_box(cache.len())
                });
            },
        );
    }

    // Insert throughput with eviction (cache smaller than data).
    let keys = random_keys(10_000);
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("insert_with_evict_10K", |b| {
        b.iter(|| {
            let mut cache: LfuCache<u64, u64> = LfuCache::new(1000);
            for &k in &keys {
                cache.insert(black_box(k), black_box(k));
            }
            black_box(cache.len())
        });
    });

    // Zipf access pattern throughput.
    let accesses = zipf_accesses(10_000, 1000);
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("zipf_get_10K", |b| {
        b.iter_batched(
            || build_full_cache(1000),
            |mut cache| {
                let mut hits = 0u64;
                for &k in &accesses {
                    if cache.get(black_box(&k)).is_some() {
                        hits += 1;
                    }
                }
                black_box(hits)
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: capacity scaling.
fn bench_capacity_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfu_cache/capacity_scaling");

    for cap in [100, 1_000, 10_000] {
        // Insert into full cache (measures eviction cost at different sizes).
        group.bench_with_input(
            BenchmarkId::new("insert_at_capacity", cap),
            &cap,
            |b, &cap| {
                b.iter_batched(
                    || build_full_cache(cap),
                    |mut cache| {
                        cache.insert(black_box(cap as u64 + 1), black_box(0));
                        cache
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        // Get from full cache.
        group.bench_with_input(
            BenchmarkId::new("get_from_full", cap),
            &cap,
            |b, &cap| {
                b.iter_batched(
                    || build_full_cache(cap),
                    |mut cache| {
                        black_box(cache.get(black_box(&(cap as u64 / 2))));
                        cache
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Benchmark: stats and serde.
fn bench_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfu_cache/stats");

    let cache = build_hot_cold_cache(1000);

    group.bench_function("stats_query", |b| {
        b.iter(|| black_box(cache.stats()));
    });

    group.bench_function("stats_serde_roundtrip", |b| {
        let stats = cache.stats();
        b.iter(|| {
            let json = serde_json::to_string(black_box(&stats)).unwrap();
            black_box(serde_json::from_str::<LfuCacheStats>(&json).unwrap())
        });
    });

    group.bench_function("config_serde_roundtrip", |b| {
        let config = LfuCacheConfig { capacity: 1000 };
        b.iter(|| {
            let json = serde_json::to_string(black_box(&config)).unwrap();
            black_box(serde_json::from_str::<LfuCacheConfig>(&json).unwrap())
        });
    });

    group.finish();
}

/// Benchmark: clear and reuse.
fn bench_clear(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfu_cache/clear");

    group.bench_function("clear_1000", |b| {
        b.iter_batched(
            || build_full_cache(1000),
            |mut cache| {
                cache.clear();
                black_box(cache)
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("clear_reinsert_100", |b| {
        b.iter_batched(
            || build_full_cache(1000),
            |mut cache| {
                cache.clear();
                for i in 0..100u64 {
                    cache.insert(black_box(i), black_box(i));
                }
                black_box(cache.len())
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── Criterion harness ──────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_insert,
    bench_get,
    bench_peek,
    bench_remove,
    bench_throughput,
    bench_capacity_scaling,
    bench_stats,
    bench_clear,
);

criterion_main!(benches);
