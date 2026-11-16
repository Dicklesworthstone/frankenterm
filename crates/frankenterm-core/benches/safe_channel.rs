//! Criterion benchmarks for the `safe_channel` module.
//!
//! Measures throughput of the reserve/commit channel against the simpler
//! try_send/try_recv path, and evaluates the overhead of the cancellation-safe
//! reservation protocol at various channel capacities.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use frankenterm_core::safe_channel::{SafeChannelConfig, safe_channel};

mod bench_common;

#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "safe_channel_single_thread",
        budget: "single-threaded send+recv roundtrip < 200 ns/op",
    },
    bench_common::BenchBudget {
        name: "safe_channel_reserve_commit",
        budget: "reserve+commit overhead < 2x try_recv baseline",
    },
];

// =============================================================================
// Single-threaded: try_send + try_recv (fire-and-forget baseline)
// =============================================================================

fn bench_try_send_recv(c: &mut Criterion) {
    let capacities: &[usize] = &[16, 64, 256, 1024];

    let mut group = c.benchmark_group("safe_channel/try_send_recv");
    for &cap in capacities {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("cap_{cap}")),
            &cap,
            |b, &cap| {
                let config = SafeChannelConfig {
                    capacity: cap,
                    max_reservations: cap,
                    ..Default::default()
                };
                let (tx, rx) = safe_channel::<u64>(config);
                b.iter(|| {
                    tx.try_send(black_box(42)).unwrap();
                    let val = rx.try_recv().unwrap();
                    black_box(val);
                });
            },
        );
    }
    group.finish();
}

// =============================================================================
// Single-threaded: try_send + try_reserve + commit (safe path)
// =============================================================================

fn bench_reserve_commit(c: &mut Criterion) {
    let capacities: &[usize] = &[16, 64, 256, 1024];

    let mut group = c.benchmark_group("safe_channel/reserve_commit");
    for &cap in capacities {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("cap_{cap}")),
            &cap,
            |b, &cap| {
                let config = SafeChannelConfig {
                    capacity: cap,
                    max_reservations: cap,
                    ..Default::default()
                };
                let (tx, rx) = safe_channel::<u64>(config);
                b.iter(|| {
                    tx.try_send(black_box(42)).unwrap();
                    let reservation = rx.try_reserve().unwrap();
                    let val = reservation.commit();
                    black_box(val);
                });
            },
        );
    }
    group.finish();
}

// =============================================================================
// Reservation rollback overhead: reserve then drop without committing
// =============================================================================

fn bench_reserve_rollback(c: &mut Criterion) {
    let mut group = c.benchmark_group("safe_channel/reserve_rollback");
    group.throughput(Throughput::Elements(1));

    let config = SafeChannelConfig {
        capacity: 64,
        max_reservations: 64,
        ..Default::default()
    };
    let (tx, rx) = safe_channel::<u64>(config);

    group.bench_function("rollback_requeue", |b| {
        b.iter(|| {
            tx.try_send(black_box(42)).unwrap();
            let reservation = rx.try_reserve().unwrap();
            // Drop without commit — item requeued to front
            drop(reservation);
            // Now recv the requeued item
            let val = rx.try_recv().unwrap();
            black_box(val);
        });
    });
    group.finish();
}

// =============================================================================
// Burst throughput: fill channel, then drain (batch pattern)
// =============================================================================

fn bench_burst_fill_drain(c: &mut Criterion) {
    let burst_sizes: &[usize] = &[16, 64, 256];

    let mut group = c.benchmark_group("safe_channel/burst");
    for &burst in burst_sizes {
        group.throughput(Throughput::Elements(burst as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("burst_{burst}")),
            &burst,
            |b, &burst| {
                let config = SafeChannelConfig {
                    capacity: burst,
                    max_reservations: burst,
                    ..Default::default()
                };
                let (tx, rx) = safe_channel::<u64>(config);
                b.iter(|| {
                    // Fill
                    for i in 0..burst as u64 {
                        tx.try_send(black_box(i)).unwrap();
                    }
                    // Drain via reserve+commit
                    for _ in 0..burst {
                        let r = rx.try_reserve().unwrap();
                        black_box(r.commit());
                    }
                });
            },
        );
    }
    group.finish();
}

// =============================================================================
// Multi-producer contention: N senders, 1 receiver
// =============================================================================

fn bench_mpsc_contention(c: &mut Criterion) {
    let producer_counts: &[usize] = &[1, 2, 4];

    let mut group = c.benchmark_group("safe_channel/mpsc");

    for &n_producers in producer_counts {
        let items_per_producer = 256;
        let total = n_producers * items_per_producer;
        group.throughput(Throughput::Elements(total as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_producers}p")),
            &n_producers,
            |b, &np| {
                let config = SafeChannelConfig {
                    capacity: total,
                    max_reservations: total,
                    ..Default::default()
                };
                b.iter(|| {
                    let (tx, rx) = safe_channel::<u64>(config.clone());
                    let handles: Vec<_> = (0..np)
                        .map(|_| {
                            let tx = tx.clone();
                            std::thread::spawn(move || {
                                for i in 0..items_per_producer as u64 {
                                    tx.try_send(i).unwrap();
                                }
                            })
                        })
                        .collect();

                    // Drain on main thread
                    let mut received = 0;
                    while received < total {
                        match rx.try_reserve() {
                            Ok(r) => {
                                black_box(r.commit());
                                received += 1;
                            }
                            Err(_) => std::thread::yield_now(),
                        }
                    }

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
    bench_try_send_recv,
    bench_reserve_commit,
    bench_reserve_rollback,
    bench_burst_fill_drain,
    bench_mpsc_contention,
);
criterion_main!(benches);
