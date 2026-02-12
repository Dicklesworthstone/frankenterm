//! Benchmark: Mutex vs sharded counters / maps under contention
//!
//! Compares traditional AtomicU64 and Mutex<HashMap> against
//! ShardedCounter and PaneMap under varying thread counts.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use frankenterm_core::concurrent_map::PaneMap;
use frankenterm_core::sharded_counter::{ShardedCounter, ShardedMax};

// ===========================================================================
// Counter benchmarks: AtomicU64 vs ShardedCounter
// ===========================================================================

fn bench_atomic_counter_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("counter_contention");
    group.sample_size(20);

    for &threads in &[1, 4, 8, 16] {
        let ops_per_thread = 100_000u64;

        // Baseline: single AtomicU64
        group.bench_with_input(
            BenchmarkId::new("AtomicU64", threads),
            &threads,
            |b, &threads| {
                let counter = Arc::new(AtomicU64::new(0));
                b.iter(|| {
                    counter.store(0, Ordering::Relaxed);
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let counter = Arc::clone(&counter);
                            std::thread::spawn(move || {
                                for _ in 0..ops_per_thread {
                                    counter.fetch_add(1, Ordering::Relaxed);
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                    assert_eq!(
                        counter.load(Ordering::Relaxed),
                        threads as u64 * ops_per_thread
                    );
                });
            },
        );

        // Baseline: single AtomicU64 with SeqCst (current RuntimeMetrics pattern)
        group.bench_with_input(
            BenchmarkId::new("AtomicU64_SeqCst", threads),
            &threads,
            |b, &threads| {
                let counter = Arc::new(AtomicU64::new(0));
                b.iter(|| {
                    counter.store(0, Ordering::SeqCst);
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let counter = Arc::clone(&counter);
                            std::thread::spawn(move || {
                                for _ in 0..ops_per_thread {
                                    counter.fetch_add(1, Ordering::SeqCst);
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                });
            },
        );

        // Sharded: ShardedCounter
        group.bench_with_input(
            BenchmarkId::new("ShardedCounter", threads),
            &threads,
            |b, &threads| {
                let counter = Arc::new(ShardedCounter::with_shards(threads.max(4)));
                b.iter(|| {
                    counter.reset();
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let counter = Arc::clone(&counter);
                            std::thread::spawn(move || {
                                for _ in 0..ops_per_thread {
                                    counter.increment();
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                    assert_eq!(counter.get(), threads as u64 * ops_per_thread);
                });
            },
        );
    }

    group.finish();
}

// ===========================================================================
// Max tracker benchmarks: AtomicU64 CAS loop vs ShardedMax
// ===========================================================================

fn bench_max_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("max_contention");
    group.sample_size(20);

    for &threads in &[1, 4, 8, 16] {
        let ops_per_thread = 50_000u64;

        // Baseline: single AtomicU64 with CAS loop
        group.bench_with_input(
            BenchmarkId::new("AtomicU64_CAS", threads),
            &threads,
            |b, &threads| {
                let max_val = Arc::new(AtomicU64::new(0));
                b.iter(|| {
                    max_val.store(0, Ordering::SeqCst);
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            let max_val = Arc::clone(&max_val);
                            std::thread::spawn(move || {
                                for j in 0..ops_per_thread {
                                    let val = t as u64 * ops_per_thread + j;
                                    let mut current = max_val.load(Ordering::SeqCst);
                                    while val > current {
                                        match max_val.compare_exchange_weak(
                                            current,
                                            val,
                                            Ordering::SeqCst,
                                            Ordering::SeqCst,
                                        ) {
                                            Ok(_) => break,
                                            Err(v) => current = v,
                                        }
                                    }
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                });
            },
        );

        // Sharded: ShardedMax
        group.bench_with_input(
            BenchmarkId::new("ShardedMax", threads),
            &threads,
            |b, &threads| {
                let max_val = Arc::new(ShardedMax::with_shards(threads.max(4)));
                b.iter(|| {
                    max_val.reset();
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            let max_val = Arc::clone(&max_val);
                            std::thread::spawn(move || {
                                for j in 0..ops_per_thread {
                                    let val = t as u64 * ops_per_thread + j;
                                    max_val.observe(val);
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                });
            },
        );
    }

    group.finish();
}

// ===========================================================================
// Map benchmarks: RwLock<HashMap> vs PaneMap
// ===========================================================================

fn bench_map_read_heavy(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_read_heavy");
    group.sample_size(20);

    for &threads in &[1, 4, 8, 16] {
        let ops_per_thread = 50_000u64;
        let pane_count = 200u64;

        // Baseline: RwLock<HashMap>
        group.bench_with_input(
            BenchmarkId::new("RwLock_HashMap", threads),
            &threads,
            |b, &threads| {
                let map = Arc::new(RwLock::new(HashMap::new()));
                // Pre-populate
                {
                    let mut guard = map.write().unwrap();
                    for i in 0..pane_count {
                        guard.insert(i, i * 10);
                    }
                }

                b.iter(|| {
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let map = Arc::clone(&map);
                            std::thread::spawn(move || {
                                for j in 0..ops_per_thread {
                                    let key = j % pane_count;
                                    if j % 100 == 0 {
                                        // 1% write
                                        let mut guard = map.write().unwrap();
                                        guard.insert(key, j);
                                    } else {
                                        // 99% read
                                        let guard = map.read().unwrap();
                                        let _ = guard.get(&key);
                                    }
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                });
            },
        );

        // Sharded: PaneMap
        group.bench_with_input(
            BenchmarkId::new("PaneMap", threads),
            &threads,
            |b, &threads| {
                let map = Arc::new(PaneMap::<u64>::with_shards(64));
                // Pre-populate
                for i in 0..pane_count {
                    map.insert(i, i * 10);
                }

                b.iter(|| {
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let map = Arc::clone(&map);
                            std::thread::spawn(move || {
                                for j in 0..ops_per_thread {
                                    let key = j % pane_count;
                                    if j % 100 == 0 {
                                        // 1% write
                                        map.insert(key, j);
                                    } else {
                                        // 99% read
                                        let _ = map.get(key);
                                    }
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                });
            },
        );
    }

    group.finish();
}

// ===========================================================================
// Map benchmarks: write-heavy scenario (pane discovery burst)
// ===========================================================================

fn bench_map_write_heavy(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_write_heavy");
    group.sample_size(20);

    for &threads in &[1, 4, 8] {
        let entries_per_thread = 1_000u64;

        // Baseline: RwLock<HashMap>
        group.bench_with_input(
            BenchmarkId::new("RwLock_HashMap", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let map = Arc::new(RwLock::new(HashMap::new()));
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            let map = Arc::clone(&map);
                            std::thread::spawn(move || {
                                for j in 0..entries_per_thread {
                                    let key = t as u64 * entries_per_thread + j;
                                    let mut guard = map.write().unwrap();
                                    guard.insert(key, key * 2);
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                });
            },
        );

        // Sharded: PaneMap
        group.bench_with_input(
            BenchmarkId::new("PaneMap", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let map = Arc::new(PaneMap::<u64>::with_shards(64));
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            let map = Arc::clone(&map);
                            std::thread::spawn(move || {
                                for j in 0..entries_per_thread {
                                    let key = t as u64 * entries_per_thread + j;
                                    map.insert(key, key * 2);
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_atomic_counter_contention,
    bench_max_contention,
    bench_map_read_heavy,
    bench_map_write_heavy
);
criterion_main!(benches);
