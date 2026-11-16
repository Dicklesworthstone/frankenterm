//! Round-5 S3-FIFO bench: scan-heavy access trace at equal cache capacity.
//!
//! The baseline arm is the default LFU policy; the candidate arm explicitly
//! selects `cache.eviction=s3fifo` through the lfucache policy API. The shipping
//! default remains LFU.

use std::fs::{OpenOptions, create_dir_all};
use std::hint::black_box;
use std::io::Write;

use config::ConfigHandle;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lfucache::{CacheEvictionPolicy, LfuCacheU64};
use serde::Serialize;

mod bench_common;

const CACHE_CAPACITY: usize = 128;
const HOT_KEYS: u64 = 32;
const SCAN_KEYS_PER_ROUND: u64 = 384;
const ROUNDS: u64 = 24;

const BUDGETS: &[bench_common::BenchBudget] = &[bench_common::BenchBudget {
    name: "lfucache_s3fifo/scan_heavy_hit_rate",
    budget: "A/B config cache.eviction=s3fifo should improve scan-resistant hit rate at equal capacity against LFU",
}];

#[derive(Debug, Clone, Copy, Serialize)]
struct HitRateMetric {
    policy: &'static str,
    hits: u64,
    misses: u64,
    evictions: u64,
    hit_rate: f64,
}

fn fixed_capacity(_: &ConfigHandle) -> usize {
    CACHE_CAPACITY
}

fn scan_trace() -> Vec<u64> {
    let mut trace =
        Vec::with_capacity(((HOT_KEYS * 3 + SCAN_KEYS_PER_ROUND) * ROUNDS + HOT_KEYS * 8) as usize);

    for _ in 0..8 {
        trace.extend(0..HOT_KEYS);
    }

    for round in 0..ROUNDS {
        trace.extend(0..HOT_KEYS);
        let scan_start = 10_000 + round * SCAN_KEYS_PER_ROUND;
        trace.extend(scan_start..scan_start + SCAN_KEYS_PER_ROUND);
        trace.extend(0..HOT_KEYS);
    }

    trace
}

fn run_trace(policy: CacheEvictionPolicy) -> HitRateMetric {
    let config = ConfigHandle::default_config();
    let mut cache = LfuCacheU64::new_with_eviction_policy(
        "round5_lfucache_hit",
        "round5_lfucache_miss",
        fixed_capacity,
        &config,
        policy,
    );
    let mut hits = 0u64;
    let mut misses = 0u64;
    let mut evictions = 0u64;

    for key in scan_trace() {
        if cache.get(&key).is_some() {
            hits = hits.saturating_add(1);
        } else {
            misses = misses.saturating_add(1);
            evictions = evictions.saturating_add(
                u64::try_from(cache.put_capturing_evictions(key, key).len()).unwrap_or(u64::MAX),
            );
        }
    }

    let total = hits.saturating_add(misses);
    let hit_rate = if total == 0 {
        0.0
    } else {
        hits as f64 / total as f64
    };

    HitRateMetric {
        policy: match policy {
            CacheEvictionPolicy::Lfu => "lfu",
            CacheEvictionPolicy::S3Fifo => "s3fifo",
        },
        hits,
        misses,
        evictions,
        hit_rate,
    }
}

fn emit_metric(metric: HitRateMetric) {
    let _ = create_dir_all("target/criterion");
    let Ok(mut out) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("target/criterion/round5-scroll-mem-cache-metrics.jsonl")
    else {
        return;
    };
    let row = serde_json::json!({
        "bench": "lfucache_s3fifo",
        "metric": metric,
    });
    let _ = writeln!(out, "{row}");
}

fn emit_metrics_once() {
    emit_metric(run_trace(CacheEvictionPolicy::Lfu));
    emit_metric(run_trace(CacheEvictionPolicy::S3Fifo));
}

fn bench_scan_heavy_hit_rate(c: &mut Criterion) {
    let mut group = c.benchmark_group("lfucache_s3fifo");

    for policy in [CacheEvictionPolicy::Lfu, CacheEvictionPolicy::S3Fifo] {
        group.bench_with_input(
            BenchmarkId::from_parameter(match policy {
                CacheEvictionPolicy::Lfu => "lfu",
                CacheEvictionPolicy::S3Fifo => "s3fifo",
            }),
            &policy,
            |b, &policy| {
                b.iter(|| black_box(run_trace(black_box(policy))));
            },
        );
    }

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    emit_metrics_once();
    bench_scan_heavy_hit_rate(c);
    bench_common::emit_bench_artifacts("lfucache_s3fifo", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
