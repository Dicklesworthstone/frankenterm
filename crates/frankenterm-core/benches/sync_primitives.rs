#![cfg(feature = "asupersync-runtime")]

//! Benchmarks for wa-3d14m sync primitive migration verification.
//!
//! Measures uncontended and contended behavior for the migrated
//! `runtime_compat::{Mutex, RwLock, Semaphore}` surface.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::cx::{CxRuntimeBuilder, RuntimeTuning, for_testing, spawn_with_cx};
use frankenterm_core::runtime_compat::{
    Mutex as CompatMutex, RwLock as CompatRwLock, Semaphore as CompatSemaphore,
};

mod bench_common;

const CONTENDED_TASKS: [usize; 3] = [2, 4, 8];
const SPIN_HOLD_ITERS: usize = 128;
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "sync_primitives/mutex_uncontended",
        budget: "uncontended mutex lock/unlock behavior",
    },
    bench_common::BenchBudget {
        name: "sync_primitives/rwlock_uncontended",
        budget: "uncontended rwlock read/write behavior",
    },
    bench_common::BenchBudget {
        name: "sync_primitives/semaphore_uncontended",
        budget: "uncontended semaphore acquire/release behavior",
    },
    bench_common::BenchBudget {
        name: "sync_primitives/mutex_contended",
        budget: "contended mutex benchmark at 2, 4, and 8 tasks",
    },
    bench_common::BenchBudget {
        name: "sync_primitives/rwlock_contended",
        budget: "contended rwlock benchmark at 2, 4, and 8 tasks",
    },
    bench_common::BenchBudget {
        name: "sync_primitives/semaphore_contended",
        budget: "contended semaphore benchmark at 2, 4, and 8 tasks",
    },
];

fn build_asupersync_current_runtime() -> asupersync::runtime::Runtime {
    CxRuntimeBuilder::current_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 1,
            poll_budget: 128,
            blocking_min_threads: 0,
            blocking_max_threads: 0,
        })
        .build()
        .expect("build current-thread asupersync benchmark runtime")
}

fn build_asupersync_multi_runtime() -> asupersync::runtime::Runtime {
    CxRuntimeBuilder::multi_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 4,
            poll_budget: 256,
            blocking_min_threads: 0,
            blocking_max_threads: 0,
        })
        .build()
        .expect("build multi-thread asupersync benchmark runtime")
}

fn bench_mutex_uncontended(c: &mut Criterion) {
    let as_runtime = build_asupersync_current_runtime();
    let as_mutex = CompatMutex::new(0usize);

    let mut group = c.benchmark_group("sync_primitives/mutex_uncontended");
    group.bench_function("asupersync", |b| {
        b.iter(|| {
            let observed = as_runtime.block_on(async {
                let mut guard = as_mutex.lock().await;
                *guard += 1;
                *guard
            });
            black_box(observed);
        });
    });
    group.finish();
}

fn bench_rwlock_uncontended(c: &mut Criterion) {
    let as_runtime = build_asupersync_current_runtime();
    let as_lock = CompatRwLock::new(0usize);

    let mut group = c.benchmark_group("sync_primitives/rwlock_uncontended");
    group.bench_function("asupersync_read", |b| {
        b.iter(|| {
            let observed = as_runtime.block_on(async {
                let guard = as_lock.read().await;
                *guard
            });
            black_box(observed);
        });
    });
    group.bench_function("asupersync_write", |b| {
        b.iter(|| {
            let observed = as_runtime.block_on(async {
                let mut guard = as_lock.write().await;
                *guard += 1;
                *guard
            });
            black_box(observed);
        });
    });
    group.finish();
}

fn bench_semaphore_uncontended(c: &mut Criterion) {
    let as_runtime = build_asupersync_current_runtime();
    let as_sem = Arc::new(CompatSemaphore::new(1));

    let mut group = c.benchmark_group("sync_primitives/semaphore_uncontended");
    group.bench_function("asupersync", |b| {
        b.iter(|| {
            let permits_left = as_runtime.block_on(async {
                let permit = CompatSemaphore::acquire_owned(Arc::clone(&as_sem))
                    .await
                    .expect("asupersync semaphore acquire");
                black_box(permit.count());
                drop(permit);
                as_sem.available_permits()
            });
            black_box(permits_left);
        });
    });
    group.finish();
}

fn bench_mutex_contended(c: &mut Criterion) {
    let as_runtime = build_asupersync_multi_runtime();
    let as_handle = as_runtime.handle();
    let as_cx = for_testing();

    let mut group = c.benchmark_group("sync_primitives/mutex_contended");
    for tasks in CONTENDED_TASKS {
        group.bench_with_input(
            BenchmarkId::new("asupersync", tasks),
            &tasks,
            |b, &tasks| {
                b.iter(|| {
                    let mutex = Arc::new(CompatMutex::new(0usize));
                    let observed = as_runtime.block_on(async {
                        let mut joins = Vec::with_capacity(tasks);
                        for _ in 0..tasks {
                            let mutex = Arc::clone(&mutex);
                            joins.push(spawn_with_cx(&as_handle, &as_cx, move |_cx| async move {
                                let mut guard = mutex.lock().await;
                                *guard += 1;
                                black_box(*guard);
                            }));
                        }
                        for join in joins {
                            join.await;
                        }
                        *mutex.lock().await
                    });
                    black_box(observed);
                });
            },
        );
    }
    group.finish();
}

fn bench_rwlock_contended(c: &mut Criterion) {
    let as_runtime = build_asupersync_multi_runtime();
    let as_handle = as_runtime.handle();
    let as_cx = for_testing();

    let mut group = c.benchmark_group("sync_primitives/rwlock_contended");
    for tasks in CONTENDED_TASKS {
        group.bench_with_input(
            BenchmarkId::new("asupersync", tasks),
            &tasks,
            |b, &tasks| {
                b.iter(|| {
                    let lock = Arc::new(CompatRwLock::new(0usize));
                    let observed = as_runtime.block_on(async {
                        let mut joins = Vec::with_capacity(tasks);

                        {
                            let lock = Arc::clone(&lock);
                            joins.push(spawn_with_cx(&as_handle, &as_cx, move |_cx| async move {
                                let mut guard = lock.write().await;
                                *guard += 1;
                                black_box(*guard);
                            }));
                        }

                        for _ in 1..tasks {
                            let lock = Arc::clone(&lock);
                            joins.push(spawn_with_cx(&as_handle, &as_cx, move |_cx| async move {
                                let guard = lock.read().await;
                                black_box(*guard);
                            }));
                        }

                        for join in joins {
                            join.await;
                        }
                        *lock.read().await
                    });
                    black_box(observed);
                });
            },
        );
    }
    group.finish();
}

fn bench_semaphore_contended(c: &mut Criterion) {
    let as_runtime = build_asupersync_multi_runtime();
    let as_handle = as_runtime.handle();
    let as_cx = for_testing();

    let mut group = c.benchmark_group("sync_primitives/semaphore_contended");
    for tasks in CONTENDED_TASKS {
        group.bench_with_input(
            BenchmarkId::new("asupersync", tasks),
            &tasks,
            |b, &tasks| {
                b.iter(|| {
                    let semaphore = Arc::new(CompatSemaphore::new(1));
                    let peak_inflight = Arc::new(AtomicUsize::new(0));
                    let inflight = Arc::new(AtomicUsize::new(0));

                    let observed = as_runtime.block_on(async {
                        let mut joins = Vec::with_capacity(tasks);
                        for _ in 0..tasks {
                            let semaphore = Arc::clone(&semaphore);
                            let inflight = Arc::clone(&inflight);
                            let peak_inflight = Arc::clone(&peak_inflight);
                            joins.push(spawn_with_cx(&as_handle, &as_cx, move |_cx| async move {
                                let permit = CompatSemaphore::acquire_owned(Arc::clone(&semaphore))
                                    .await
                                    .expect("asupersync semaphore task acquire");
                                let current = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                                peak_inflight.fetch_max(current, Ordering::SeqCst);
                                for _ in 0..SPIN_HOLD_ITERS {
                                    std::hint::spin_loop();
                                }
                                inflight.fetch_sub(1, Ordering::SeqCst);
                                black_box(permit.count());
                                drop(permit);
                            }));
                        }
                        for join in joins {
                            join.await;
                        }
                        peak_inflight.load(Ordering::SeqCst)
                    });
                    black_box(observed);
                });
            },
        );
    }
    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("sync_primitives", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_mutex_uncontended,
        bench_rwlock_uncontended,
        bench_semaphore_uncontended,
        bench_mutex_contended,
        bench_rwlock_contended,
        bench_semaphore_contended
);
criterion_main!(benches);
