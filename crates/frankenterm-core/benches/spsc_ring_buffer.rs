//! Criterion benchmarks for SPSC ring-buffer throughput.
//!
//! Measures steady-state enqueue/dequeue throughput in:
//! - single-thread roundtrip (producer+consumer in one thread)
//! - two-thread stream (dedicated producer and consumer threads)

use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::thread;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::spsc_ring_buffer::channel;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "spsc_ring/single_thread_roundtrip/capacity_1024",
        budget: "1M enqueue+dequeue operations with cap=1024",
    },
    bench_common::BenchBudget {
        name: "spsc_ring/two_thread_stream/capacity_1024",
        budget: "500k produced/consumed values with dedicated producer and consumer",
    },
];

fn bench_single_thread_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc_ring/single_thread_roundtrip");
    const OPS: u64 = 1_000_000;
    group.throughput(Throughput::Elements(OPS));

    for &capacity in &[64usize, 1024, 8192] {
        group.bench_with_input(
            BenchmarkId::new("capacity", capacity),
            &capacity,
            |b, &cap| {
                b.iter(|| {
                    let (tx, rx) = channel::<u64>(cap);
                    for i in 0..OPS {
                        let mut next = i;
                        loop {
                            match tx.try_send(next) {
                                Ok(()) => break,
                                Err(v) => {
                                    next = v;
                                    std::hint::spin_loop();
                                }
                            }
                        }

                        loop {
                            if let Some(v) = rx.try_recv() {
                                black_box(v);
                                break;
                            }
                            std::hint::spin_loop();
                        }
                    }
                });
            },
        );
    }

    group.finish();
}

fn bench_two_thread_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc_ring/two_thread_stream");
    const OPS: u64 = 500_000;
    group.throughput(Throughput::Elements(OPS));

    for &capacity in &[256usize, 1024, 4096] {
        group.bench_with_input(
            BenchmarkId::new("capacity", capacity),
            &capacity,
            |b, &cap| {
                b.iter(|| {
                    let (tx, rx) = channel::<u64>(cap);
                    let barrier = Arc::new(Barrier::new(2));
                    let producer_barrier = Arc::clone(&barrier);

                    let producer = thread::spawn(move || {
                        producer_barrier.wait();
                        for i in 0..OPS {
                            let mut next = i;
                            loop {
                                match tx.try_send(next) {
                                    Ok(()) => break,
                                    Err(v) => {
                                        next = v;
                                        std::hint::spin_loop();
                                    }
                                }
                            }
                        }
                        tx.close();
                    });

                    barrier.wait();

                    let mut checksum = 0u64;
                    loop {
                        if let Some(v) = rx.try_recv() {
                            checksum = checksum.wrapping_add(v);
                            continue;
                        }
                        if rx.is_closed() {
                            break;
                        }
                        std::hint::spin_loop();
                    }

                    producer.join().expect("producer thread failed");
                    black_box(checksum);
                });
            },
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("spsc_ring_buffer", BUDGETS);
    Criterion::default()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_single_thread_roundtrip, bench_two_thread_stream
);
criterion_main!(benches);
