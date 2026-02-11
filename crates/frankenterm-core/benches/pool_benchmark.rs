//! Benchmarks for the generic async connection pool.
//!
//! Performance budgets:
//! - Pool construction: **< 10us**
//! - Acquire + return cycle: **< 50us**
//! - try_acquire (non-blocking): **< 10us**
//! - Stats collection: **< 5us**
//! - Idle eviction (100 conns): **< 100us**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::pool::{Pool, PoolConfig};
use std::time::Duration;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "pool_construct",
        budget: "p50 < 10us (pool construction)",
    },
    bench_common::BenchBudget {
        name: "acquire_return",
        budget: "p50 < 50us (acquire + put cycle)",
    },
    bench_common::BenchBudget {
        name: "try_acquire",
        budget: "p50 < 10us (non-blocking acquire)",
    },
    bench_common::BenchBudget {
        name: "pool_stats",
        budget: "p50 < 5us (stats snapshot)",
    },
    bench_common::BenchBudget {
        name: "idle_eviction",
        budget: "p50 < 100us (evict 100 idle conns)",
    },
];

/// Dummy connection type for benchmarking.
#[derive(Debug)]
#[allow(dead_code)]
struct DummyConn(u64);

fn make_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn bench_pool_construct(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool/construct");

    for &max_size in &[4, 16, 64] {
        group.bench_with_input(
            BenchmarkId::from_parameter(max_size),
            &max_size,
            |b, &size| {
                b.iter(|| {
                    let config = PoolConfig {
                        max_size: size,
                        idle_timeout: Duration::from_secs(300),
                        acquire_timeout: Duration::from_secs(5),
                    };
                    Pool::<DummyConn>::new(config)
                });
            },
        );
    }

    group.finish();
}

fn bench_acquire_return(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool/acquire_return");
    let rt = make_runtime();

    for &max_size in &[1, 4, 16] {
        let config = PoolConfig {
            max_size,
            idle_timeout: Duration::from_secs(300),
            acquire_timeout: Duration::from_secs(5),
        };

        group.bench_with_input(
            BenchmarkId::from_parameter(max_size),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    let pool = Pool::<DummyConn>::new(config.clone());
                    let result = pool.acquire().await.unwrap();
                    // Decompose via public API
                    let (conn, _guard) = result.into_parts();
                    let conn = conn.unwrap_or(DummyConn(42));
                    // Return connection to pool
                    pool.put(conn).await;
                });
            },
        );
    }

    group.finish();
}

fn bench_acquire_with_idle(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool/acquire_with_idle");
    let rt = make_runtime();

    for &max_size in &[4, 16] {
        let config = PoolConfig {
            max_size,
            idle_timeout: Duration::from_secs(300),
            acquire_timeout: Duration::from_secs(5),
        };

        group.bench_with_input(
            BenchmarkId::from_parameter(max_size),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    let pool = Pool::<DummyConn>::new(config.clone());
                    // Pre-fill idle queue
                    pool.put(DummyConn(1)).await;
                    // Acquire should get the idle connection
                    let result = pool.acquire().await.unwrap();
                    assert!(result.has_connection());
                    let (conn, _guard) = result.into_parts();
                    drop(conn);
                });
            },
        );
    }

    group.finish();
}

fn bench_try_acquire(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool/try_acquire");
    let rt = make_runtime();

    let config = PoolConfig {
        max_size: 4,
        idle_timeout: Duration::from_secs(300),
        acquire_timeout: Duration::from_secs(5),
    };

    group.bench_function("available_slot", |b| {
        b.to_async(&rt).iter(|| async {
            let pool = Pool::<DummyConn>::new(config.clone());
            let result = pool.try_acquire().await.unwrap();
            drop(result);
        });
    });

    group.finish();
}

fn bench_pool_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool/stats");
    let rt = make_runtime();

    for &max_size in &[4, 16, 64] {
        let config = PoolConfig {
            max_size,
            idle_timeout: Duration::from_secs(300),
            acquire_timeout: Duration::from_secs(5),
        };

        group.bench_with_input(
            BenchmarkId::from_parameter(max_size),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    let pool = Pool::<DummyConn>::new(config.clone());
                    pool.stats().await
                });
            },
        );
    }

    group.finish();
}

fn bench_idle_eviction(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool/idle_eviction");
    let rt = make_runtime();

    for &count in &[10, 50, 100] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.to_async(&rt).iter(|| async {
                let config = PoolConfig {
                    max_size: count + 10,
                    // Zero timeout so everything is instantly evictable
                    idle_timeout: Duration::from_secs(0),
                    acquire_timeout: Duration::from_secs(5),
                };
                let pool = Pool::<DummyConn>::new(config);
                // Fill idle queue
                for i in 0..count {
                    pool.put(DummyConn(i as u64)).await;
                }
                // Evict all
                pool.evict_idle().await
            });
        });
    }

    group.finish();
}

fn bench_pool_clear(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool/clear");
    let rt = make_runtime();

    for &count in &[10, 50, 100] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.to_async(&rt).iter(|| async {
                let config = PoolConfig {
                    max_size: count + 10,
                    idle_timeout: Duration::from_secs(300),
                    acquire_timeout: Duration::from_secs(5),
                };
                let pool = Pool::<DummyConn>::new(config);
                for i in 0..count {
                    pool.put(DummyConn(i as u64)).await;
                }
                pool.clear().await;
            });
        });
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("pool_benchmark", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_pool_construct,
        bench_acquire_return,
        bench_acquire_with_idle,
        bench_try_acquire,
        bench_pool_stats,
        bench_idle_eviction,
        bench_pool_clear
);
criterion_main!(benches);
